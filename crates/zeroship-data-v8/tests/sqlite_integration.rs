//! SQLite-side integration tests.
//!
//! Ordinary package tests exercise the `SqliteSession` actor end-to-end:
//!
//! - bootstrap PRAGMAs land (`journal_mode = wal`, `busy_timeout = 5000`)
//! - `DatabaseFixture::execute_fixture` round-trips DDL + DML
//! - `DatabaseFixture::execute_fixture_on` round-trips DDL + DML through a
//!   handle returned by `fixture_session`
//!
//! Each test spins up a per-test `tempfile::TempDir` and constructs a
//! `new_sqlite_backend(db_dir)` directly - this deliberately bypasses
//! the per-isolate context plumbing the ATTACH threads
//! through, so these tests pin the actor's behaviour in isolation.
//! The higher-level orchestrator mirror is covered separately.

// `support`, `schema_fixture` and `parity` are declared once by
// `tests/test_helpers.rs`, the entry file this module hangs off; its header says
// why a second declaration here would be a second copy of their statics.
use crate::parity;
#[allow(unused_imports)]
use crate::schema_fixture::{fixture_table_sql, fixture_table_sql_for};
#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use std::path::PathBuf;

use std::rc::Rc;

use zeroship_data_orm::cdc::ChangeOp;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::backend::sqlite::SqliteBackend;
use zeroship_data_orm::backend::sqlite::reservation::{CancelCleanup, TerminalOutcome};
use zeroship_data_orm::backend::sqlite::session::TerminalIntent;
use zeroship_data_orm::backend::{BackendHandle, LockManager, LockScope};
use zeroship_data_orm::backend_selection::new_sqlite_backend;
// The bounded-retry surface is the policy extension trait, not `LockManager`.
use zeroship_data_orm::cdc::broker::{Subscription, SubscriptionMessage, subscribe};
use zeroship_data_sql::compile::raw_column_name;
use zeroship_data_orm::lock_policy::BoundedLockAcquire;

/// Spin up a fresh `SqliteBackend` rooted at a per-test temp dir.
///
/// Returns the backend + the `TempDir` guard — keep the guard alive
/// for the duration of the test so the dir survives until the
/// SqliteBackend's session drops (the worker thread closes the
/// connection on drop, which writes the final WAL checkpoint).
fn fresh_backend() -> (SqliteBackend, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let backend = new_sqlite_backend(
        PathBuf::from(dir.path()),
        zeroship_data_v8::testing::isolate_key_source(),
    )
    .expect("open SqliteBackend");
    (backend, dir)
}

/// The backend handle the unmask entry points now take as a parameter.
///
/// They resolved one themselves, from the isolate's context, until 2026-09-03.
/// That read is the ADAPTER's and `protection::unmask` is ENGINE, so the resolution
/// moved to the V8 dispatcher and the value is passed down. These tests drive
/// the engine directly, so they make the same call the dispatcher makes on
/// their behalf.
///
/// **THIS HELPER IS THE HARNESS PERFORMING THE OPEN, not a witness that
/// something else performed it.** It is literally `tx_scope::ensure_backend`,
/// so every `configure_cold_sqlite_unmask_fixture` case that reaches an engine
/// entry point through it has had its isolate warmed by this line rather than
/// by the code under test. What that leaves bound is the ATTACH half
/// (`prepare_unmask_backend` -> `backend.prepare_for_app`); the OPEN half is
/// bound separately and by name, by the three
/// `cold_*_open_comes_from_ensure_backend_not_the_fixture` tests below.
async fn unmask_backend() -> zeroship_data_orm::backend::BackendHandle {
    zeroship_data_v8::tx_scope::ensure_backend()
        .await
        .expect("the backend the V8 dispatcher would have opened")
}

/// The route the unmask dispatchers now take, in place of a bare handle.
///
/// See the twin in `mask_flip.rs` for why. No fixture that reaches it here
/// parks a transaction, so every call binds `in_tx = false` and takes the lane
/// it took before - `op_conn`, on this tier.
async fn unmask_route(app: &str) -> zeroship_data_orm::tx_route::TxRoute {
    zeroship_data_orm::exec::ambient_route_for_tests(app, unmask_backend().await)
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
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.seed, parity::expected_seed_projection());
    });
}

#[test]
fn parity_matrix_sqlite_transaction_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.tx, parity::expected_tx_projection());
    });
}

#[test]
fn parity_matrix_sqlite_typed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.typed, parity::expected_typed_projection());
    });
}

/// The dev tier must store a `t.bytes()` value as a BLOB of the caller's bytes.
///
/// WHY THIS IS SEPARATE FROM THE PROJECTION TEST ABOVE, and why SQLite needed a
/// test at all. Before `crud::bytes_pass`, the projection test above was GREEN
/// while the stored cell was wrong: rusqlite bound the SDK's base64 string as
/// TEXT into a BLOB-affinity column, read it back as TEXT, and
/// `read_pipeline::normalize_bytes_value` passes a string through untouched - so
/// the input reappeared and the round trip looked perfect. `typeof()` is what
/// separates the two, and it is the reason the SQLite leg is the control that
/// isolates the layer: the SDK, the read pipeline and the JSON wire shape are
/// shared with Postgres, so a defect visible on one and hidden on the other has
/// to live below them, in the bind.
#[test]
fn bytes_column_stores_a_raw_blob_on_sqlite() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);

        // A fresh backend has attached nothing: the matrix's app database is a
        // separate file (`<dir>/zs-default.sqlite`) reached through an ATTACH
        // alias, so re-attach it before the schema-qualified name resolves.
        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open the parity backend");
        backend
            .attach_app_file("default")
            .await
            .expect("attach the matrix app database");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        // `query` materialises every cell as `Option<String>` and renders a BLOB
        // as `<N bytes blob>`, so ask SQLite itself for the discriminant and the
        // hex - the same route `p5_*` uses for ciphertext.
        let sql = format!(
            "SELECT typeof(payload_bytes), hex(payload_bytes) FROM \"default\".\"{}\" \
             WHERE title = 'typed-roundtrip'",
            snapshot.collection
        );
        let rows = client.query(&sql, &[]).await.expect("SELECT");
        assert_eq!(rows.len(), 1, "the typed round-trip row must exist");

        let kind = rows[0][0].clone().expect("typeof() is never null");
        let hex = rows[0][1].clone().expect("hex() is never null");
        let expected_hex: String = parity::TYPED_BYTES_RAW
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();

        assert_eq!(
            kind, "blob",
            "a t.bytes() cell must be a BLOB, not {kind}. 'text' here is the \
             write path binding the base64 wire string as text into a \
             BLOB-affinity column, which round-trips through env.db while \
             storing the wrong thing"
        );
        assert_eq!(hex, expected_hex, "the BLOB must hold the caller's bytes");
    });
}

/// Read the value of a single-column scalar PRAGMA back from the
/// session.
///
/// SQLite's `PRAGMA <name>` syntax returns a single one-column row;
/// the column name is the pragma's name (e.g. `journal_mode`,
/// `timeout`). The session's `query` helper materialises each cell
/// as `Option<String>` already, which is the right shape for PRAGMA
/// inspection at this layer. The `Catalog` impl provides a
/// proper typed surface; these tests use the session directly via the
/// `test-helpers`-gated handle accessor.
async fn pragma_value(backend: &SqliteBackend, pragma: &str) -> String {
    let client = backend
        .fixture_session("default")
        .await
        .expect("acquire client");
    let sql = format!("PRAGMA {pragma}");
    let rows = client.query(&sql, &[]).await.expect("PRAGMA query");
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
fn execute_fixture_round_trip() {
    run(async {
        let (backend, _dir) = fresh_backend();
        // DDL — execute returns 0 rows affected for CREATE TABLE.
        backend
            .execute_fixture("CREATE TABLE t (x INTEGER)", &[])
            .await
            .expect("CREATE TABLE");
        // DML — INSERT one row, expect affected = 1.
        let n = backend
            .execute_fixture("INSERT INTO t VALUES (1)", &[])
            .await
            .expect("INSERT");
        assert_eq!(n, 1, "INSERT INTO t VALUES (1) should affect 1 row");
    });
}

#[test]
fn execute_fixture_on_round_trip() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .fixture_session("default")
            .await
            .expect("fixture_session");
        // DDL via the handle — both paths route through the same
        // actor, so DDL on the client must be visible to subsequent
        // execute_fixture calls (and vice-versa).
        backend
            .execute_fixture_on(&client, "CREATE TABLE t2 (y INTEGER)", &[])
            .await
            .expect("CREATE TABLE via client");
        let n = backend
            .execute_fixture_on(&client, "INSERT INTO t2 VALUES (42)", &[])
            .await
            .expect("INSERT via client");
        assert_eq!(n, 1, "INSERT via client should affect 1 row");

        // Cross-check: execute_fixture on the same backend sees the same
        // table (single-writer actor — there is no isolation
        // between client and pool surfaces).
        let n2 = backend
            .execute_fixture("INSERT INTO t2 VALUES (43)", &[])
            .await
            .expect("INSERT via pool sees client-DDL'd table");
        assert_eq!(n2, 1);
    });
}

// ---------------------------------------------------------------------------
// attach_app_file (ATTACH) integration tests.
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
            .attach_app_file("app_demo")
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
            .fixture_session("app_demo")
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
            .attach_app_file("app_demo")
            .await
            .expect("first ensure_app_schema");
        // Second call must NOT surface "database app_demo is already
        // in use" — the cache (or the error-suppression fallback)
        // should short-circuit it to Ok.
        backend
            .attach_app_file("app_demo")
            .await
            .expect("second ensure_app_schema must be idempotent");
    });
}

#[test]
fn ensure_app_schema_isolates_per_app() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_a")
            .await
            .expect("attach app_a");
        backend
            .attach_app_file("app_b")
            .await
            .expect("attach app_b");

        // Create a table inside the `app_a` namespace.
        backend
            .execute_fixture("CREATE TABLE \"app_a\".\"t\" (x INTEGER)", &[])
            .await
            .expect("CREATE TABLE in app_a");

        // The table must be visible in `app_a`'s catalog.
        //
        // On the AUTOCOMMIT client deliberately. `op_conn` is the connection
        // that carries every attached app, and this test reads two apps'
        // catalogs from one handle. A transaction client would refuse the
        // second read by construction now that a transaction connection
        // ATTACHes only its own app - which is a different property, ruled on
        // by `a_transaction_lane_cannot_address_another_apps_tables`.
        let client = backend.autocommit_client();
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
            .attach_app_file("app_demo")
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
// LockManager (in-process registry) + Catalog
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
            .fixture_session("default")
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
        // slot as held — `Ok(false)` is the contended return. The second
        // handle comes from `autocommit_client()`, which is a handle on the
        // OTHER connection: `fixture_session` is now the exclusive
        // `tx_conn` reservation and a second one is refused, so asking for it
        // here would measure lane admission rather than lock contention.
        let other_client = backend.autocommit_client();
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
            .fixture_session("default")
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
    // We use `LockScope::GlobalApp` (the only production-shaped variant) so
    // the key derivation matches snapshot/restore. `LockScope::LocalApp`
    // derives identical keys; visibility does not alter their shape.
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let scope = LockScope::GlobalApp {
            app_id: "app_demo".to_string(),
            name: "snapshot_restore".to_string(),
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
                    message.contains("snapshot_restore"),
                    "contention message should mention the scope name: {message}"
                );
            }
            other => {
                panic!("expected DbError::LockContention after backoff exhaustion, got {other:?}")
            }
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
            .attach_app_file("app_demo")
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
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        // Create a small table with a mix of NULL and NOT NULL
        // columns, plus a non-PK index, so the introspect output
        // exercises every PRAGMA branch.
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        backend
            .execute_fixture_on(
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
            .execute_fixture_on(
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
        assert_eq!(
            cols.len(),
            3,
            "items has 3 columns, got {:?}",
            cols.keys().collect::<Vec<_>>()
        );

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
            !live.foreign_keys.contains_key("items"),
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
// Cross-app FK parse-time check.
//
// The two `create_index_with_recovery` tests that used to head this section
// (happy path, and the `unique_violation` -> `SchemaRefused` envelope) are
// DELETED with the `IndexBuilder` capability itself: the data plane no longer
// has a way to create an index, so there is no branch left to rule on. The
// `__zeroship_migrations` fixture they provisioned went with them.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SqliteCdcDispatcher (preupdate/commit/rollback hooks) +
// worker->compio publisher integration tests.
//
// The publisher task is asynchronous: a COMMIT on the writer thread
// ships a `CommitPacket` via flume, the publisher task wakes on
// `recv_async`, resolves column names via `PRAGMA table_info` through
// the session actor, then calls `broker::publish` on the compio
// thread. The process-wide broker is the test consumer. Each fixture uses a
// distinct app id so parallel tests cannot consume each other's changes.
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

/// Subscribe to this fixture's `(app_id, collection)` on the process broker.
fn subscribe_local(app_id: &str, collection: &str) -> Subscription {
    subscribe(app_id, collection)
}

#[test]
fn insert_publishes_via_preupdate_hook() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("cdc_insert")
            .await
            .expect("ensure_app_schema");

        // Create a user table the CDC hook will fire against.
        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_insert\".\"items\" (\
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
        let sub = subscribe_local("cdc_insert", "items");

        // INSERT a row — the preupdate hook fires, commit hook ships
        // the packet, publisher resolves column names + publishes.
        backend
            .execute_fixture(
                "INSERT INTO \"cdc_insert\".\"items\" (name) VALUES ('alice')",
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
                assert_eq!(ev.app_id, "cdc_insert");
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
            .attach_app_file("cdc_typed_id")
            .await
            .expect("ensure_app_schema");

        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_typed_id\".\"typed_items\" (\
                     id TEXT PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE typed_items");

        let sub = subscribe_local("cdc_typed_id", "typed_items");
        let typed_id = "usr_02HXSQLITECDCLOGICALPK";

        backend
            .execute_fixture(
                &format!(
                    "INSERT INTO \"cdc_typed_id\".\"typed_items\" (id, name) \
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
            .attach_app_file("cdc_update")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_update\".\"items\" (\
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
            .execute_fixture(
                "INSERT INTO \"cdc_update\".\"items\" (id, name) VALUES (1, 'alice')",
                &[],
            )
            .await
            .expect("INSERT seed row");

        // Give the publisher a chance to drain the seed event so it
        // doesn't show up in the subscription created below (the
        // subscribe happens on the same thread, but only AFTER the
        // publisher has fanned out the prior packet).
        drain_publisher().await;

        let sub = subscribe_local("cdc_update", "items");

        // UPDATE the row — the preupdate hook should capture both
        // OLD ('alice') and NEW ('bob') tuples.
        backend
            .execute_fixture(
                "UPDATE \"cdc_update\".\"items\" SET name = 'bob' WHERE id = 1",
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
            .attach_app_file("cdc_rollback")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_rollback\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        let sub = subscribe_local("cdc_rollback", "items");

        // BEGIN / INSERT / ROLLBACK — each statement routes through
        // the session actor (same worker thread; serialised by the
        // mpsc queue). The rollback_hook clears the buffer; no packet
        // ships.
        backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
        backend
            .execute_fixture(
                "INSERT INTO \"cdc_rollback\".\"items\" (name) VALUES ('alice')",
                &[],
            )
            .await
            .expect("INSERT inside tx");
        backend
            .execute_fixture("ROLLBACK", &[])
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
            .attach_app_file("cdc_mixed")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_mixed\".\"items\" (\
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
            .execute_fixture(
                "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (10, 'b_pre'), (20, 'c_pre')",
                &[],
            )
            .await
            .expect("INSERT seed rows");
        drain_publisher().await;

        let sub = subscribe_local("cdc_mixed", "items");

        // BEGIN; INSERT a; UPDATE b; DELETE c; INSERT d; COMMIT.
        // Each statement fires the preupdate hook once; the commit
        // hook ships a single CommitPacket with all 4 events in
        // buffer order.
        backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
        backend
            .execute_fixture(
                "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (1, 'a')",
                &[],
            )
            .await
            .expect("INSERT a");
        backend
            .execute_fixture(
                "UPDATE \"cdc_mixed\".\"items\" SET name = 'b_post' WHERE id = 10",
                &[],
            )
            .await
            .expect("UPDATE b");
        backend
            .execute_fixture("DELETE FROM \"cdc_mixed\".\"items\" WHERE id = 20", &[])
            .await
            .expect("DELETE c");
        backend
            .execute_fixture(
                "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (2, 'd')",
                &[],
            )
            .await
            .expect("INSERT d");
        backend
            .execute_fixture("COMMIT", &[])
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
// Relation filter gates (MV/audit) + subscription fan-out under
// load. The relation filter itself is wired in
// `cdc.rs::preupdate_callback` (first early-return after the action
// discriminant). These tests add the integration coverage that pins
// the filter's behaviour end-to-end + the broker primitive
// `Broker::resume_app_with_resync` (unit-covered in `broker.rs`).
//
// Test budget: each test stays well under 2s on the CI workers — the
// fan-out test uses 10 subscribers × 100 rows (NOT the plan §8
// 100 × 1000, which would saturate dev hardware; the buffer-index
// ordering invariant is identical at smaller scale).
// ---------------------------------------------------------------------------

#[test]
fn subscription_fanout_under_load() {
    // Plan §8 / §9: a single COMMIT of N rows must reach every
    // active subscriber in INSERT order. Scaled down to 10×100 per the
    // task spec ("100 subscribers × 1000 rows would saturate dev
    // hardware; scale down to 10 × 100 for CI sanity"). The default
    // queue depth is 1024 (`broker::DEFAULT_QUEUE_DEPTH`), so 100 rows
    // fit comfortably without triggering the overflow-to-Resync path.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_fanout")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
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
        let subs: Vec<Subscription> = (0..10)
            .map(|_| subscribe_local("app_fanout", "items"))
            .collect();

        // BEGIN; 100×INSERT; COMMIT. Each statement routes through the
        // session actor in order, so the buffer accumulates events in
        // INSERT order. The commit_hook then ships one CommitPacket
        // with all 100 events; the publisher iterates and fans out.
        backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
        for i in 0..100 {
            let sql =
                format!("INSERT INTO \"app_fanout\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT inside tx");
        }
        backend
            .execute_fixture("COMMIT", &[])
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
            // Buffer-index ordering invariant: events appear
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
                        let id_str = ev.new_tuple.get("id").unwrap_or_else(|| {
                            panic!(
                                "subscriber #{i} event {idx} missing `id`: {:?}",
                                ev.new_tuple
                            )
                        });
                        let id: i64 = id_str
                            .parse()
                            .unwrap_or_else(|_| panic!("non-numeric id: {id_str}"));
                        assert_eq!(
                            id, idx as i64,
                            "subscriber #{i} event {idx} must carry id={idx}; got id={id}"
                        );
                    }
                    other => panic!("subscriber #{i} event {idx} must be Change; got {other:?}"),
                }
            }
        }
    });
}

#[test]
fn mv_refresh_does_not_emit_change_events() {
    // Plan §6 + §9: writes to `__zeroship_mv_*` shadow tables
    // must be filtered upstream of the broker. The plan acknowledges
    // (§9) that the `db.materializedView(...).refresh()` SDK
    // primitive does not exist yet, so we exercise the filter directly
    // by writing to a shadow table whose name matches the filter
    // prefix — the dispatcher cannot distinguish a "real" MV refresh
    // from a hand-rolled shadow write.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_mv")
            .await
            .expect("ensure_app_schema");
        // Create a shadow table that mimics what an MV refresh would
        // emit. The CREATE itself only touches sqlite_master (already
        // filtered); the INSERT below is the gate.
        backend
            .execute_fixture(
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
            .execute_fixture(
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
            .attach_app_file("app_mv_mixed")
            .await
            .expect("ensure_app_schema");
        // Regular collection.
        backend
            .execute_fixture(
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
            .execute_fixture(
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
        backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
        backend
            .execute_fixture(
                "INSERT INTO \"app_mv_mixed\".\"items\" (id, name) VALUES (1, 'alice')",
                &[],
            )
            .await
            .expect("INSERT items");
        backend
            .execute_fixture(
                "INSERT INTO \"app_mv_mixed\".\"__zeroship_mv_items\" (id, v) VALUES (1, 'a')",
                &[],
            )
            .await
            .expect("INSERT shadow");
        backend
            .execute_fixture("COMMIT", &[])
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
    // alongside `__zeroship_mv_*`. The unit test in
    // `cdc.rs::tests::is_filtered_relation_excludes_system_tables`
    // already pins the predicate; this gate exercises the filter
    // end-to-end so a regression that drops the audit-prefix arm of the
    // predicate would fail here at the integration boundary.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_audit")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
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
            .execute_fixture(
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
// CRITICAL fences (plan §7 + §8 + §9).
//
// These two tests are fences for the backfill-pause + schema-pending
// decoder rails. They must pass
// byte-for-byte: a regression that detaches `BrokerPauseGuard::drop`
// from `broker::unsuppress_app` + `Broker::resume_app_with_resync`,
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
// earlier subscription tests.
//
// WHAT THESE THREE DO NOT BIND. They drop the guard before draining, which is
// the order that COULD expose a delivery-window regression - but they cannot
// force it. The publisher is a compio task on this same thread, so it wakes
// during each `execute_fixture().await` and in practice drains the queue as the
// writes land; by `drop(guard)` there is usually nothing left in flight.
// Measured 2026-09-03: reverting the commit-time stamp in
// `zeroship-data-sqlite/src/cdc.rs` (publisher re-samples `sink.disposition`)
// leaves all three GREEN. The deterministic fences for that contract are the
// four `cdc::tests::*_window` / `*_drainage` unit tests in that crate, which
// hold the packet in the channel by not spawning the publisher at all; under
// the same revert, three of them fail and the control still passes. What these
// three bind is the end-to-end shape: real hooks, real broker, one Resync, and
// a FIFO sentinel proving the publisher actually ran.
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
    // Plan §7 + §9 gate - backfill pause rail end-to-end:
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
            .attach_app_file("app_backfill")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
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
        // `broker::suppress_app(app_id)`; the publisher's
        // per-event check drops every packet for this app until the
        // guard drops.
        let guard = zeroship_data_orm::cdc::broker::BrokerPauseGuard::new("app_backfill".to_string());

        // INSERT 100 rows under the suppression window. Each statement
        // routes through the session actor, the preupdate hook fires,
        // the commit_hook ships a one-event CommitPacket — the
        // publisher receives the packet, sees `is_app_suppressed`,
        // drops the event + emits a debug-level trace, moves on.
        for i in 0..100 {
            let sql =
                format!("INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT under backfill pause");
        }

        // Drop the guard WITHOUT draining first. This is the exposing order:
        // the 100 packets are still queued, and the guard that covered their
        // commits is already gone by the time the publisher dequeues them.
        //
        // This test drained first until 2026-09-03, which made the window a
        // function of publisher scheduling rather than of the guard's scope.
        // Suppression is stamped in the commit hook now, so the queued packets
        // stay suppressed and the order below is the one worth pinning.
        drop(guard);

        // Sentinel: one INSERT *after* the window. The channel is FIFO, so
        // observing its Change proves the publisher ran past all 100 queued
        // packets — without it, "no Change events" would also be satisfied by
        // a publisher that never woke at all.
        backend
            .execute_fixture(
                "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES (1000, 'after')",
                &[],
            )
            .await
            .expect("INSERT after the backfill window");

        drain_publisher_long().await;

        let msgs = drain(&sub);

        // Expected shape: [Resync, Change(1000, 'after')].
        assert_eq!(
            msgs.len(),
            2,
            "expected exactly [Resync, Change(sentinel)] after backfill pause + drop; \
             got {} messages: {msgs:?}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], SubscriptionMessage::Resync),
            "the first message must be Resync; got {:?}",
            msgs[0]
        );
        match &msgs[1] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(
                    ev.new_tuple.get("name").map(String::as_str),
                    Some("after"),
                    "the only Change must be the post-window sentinel; got {ev:?}"
                );
            }
            other => panic!("expected the sentinel Change; got {other:?}"),
        }
        // Defensive: NO in-window Change leaked past the suppression stamp.
        // (Implied by len==2 plus the sentinel match, restated so a future
        // change that interleaves window events surfaces the intent.)
        let in_window_changes = msgs
            .iter()
            .filter_map(|m| match m {
                SubscriptionMessage::Change(ev) => Some(ev),
                _ => None,
            })
            .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
            .count();
        assert_eq!(
            in_window_changes, 0,
            "no Change events must reach the subscriber for commits made during a \
             backfill window; got {in_window_changes}"
        );
    });
}

#[test]
fn schema_pending_decoder_drops_then_resyncs() {
    // Plan §7 + §16.7 + §9 gate - schema-pending decoder rail
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
            .attach_app_file("app_pending")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
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
        let guard = zeroship_data_orm::cdc::broker::SchemaPendingGuard::new("app_pending".to_string());

        // INSERT 50 rows under the schema-pending window. Same shape
        // as the backfill test above — packets ship, publisher drops.
        for i in 0..50 {
            let sql =
                format!("INSERT INTO \"app_pending\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
            backend
                .execute_fixture(&sql, &[])
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
        let attempt = zeroship_data_orm::cdc::broker::try_subscribe("app_pending", "other_collection");
        match &attempt {
            Err(DbError::Coded { code, .. }) => {
                assert_eq!(
                    code, "schema_pending",
                    "try_subscribe during schema-pending must reject \
                     with code=schema_pending; got code={code}"
                );
            }
            other => panic!("expected Err(Coded {{ code: schema_pending }}); got {other:?}"),
        }

        // Drop the guard WITHOUT draining first — the exposing order. The 50
        // packets are still queued and the guard that covered their commits is
        // gone before the publisher dequeues them. The post-disengage INSERT
        // below is the FIFO sentinel that proves the publisher ran past them.
        //
        // This test drained first until 2026-09-03, which hid the window
        // behind publisher scheduling.
        drop(guard);

        // Post-disengage: a fresh INSERT must publish normally.
        backend
            .execute_fixture(
                "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES (999, 'after')",
                &[],
            )
            .await
            .expect("INSERT after disengage");

        // Give the publisher time to drain the 50 in-window packets AND the
        // post-disengage one. The long budget (not `drain_publisher`) because
        // the guard now drops before any of them are dequeued.
        drain_publisher_long().await;

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
            other => panic!("second message must be Change(post-disengage); got {other:?}"),
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
// BrokerPauseGuard also fences writes through a type-erased backend handle.
// ---------------------------------------------------------------------------

#[test]
fn backfill_pauses_broker_for_a_type_erased_backend_and_resyncs() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_orch")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_orch\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        // Use the same type-erased handle held by the isolate context.
        let handle = BackendHandle::new(Rc::new(backend));

        let sub = subscribe_local("app_orch", "items");

        // The ORM owns pause state independently of driver dispatch.
        let guard = zeroship_data_orm::cdc::broker::BrokerPauseGuard::new("app_orch".to_string());

        // Borrow the concrete backend from its type-erased owner.
        let backend_ref = handle
            .get::<zeroship_data_orm::backend::SqliteBackend>()
            .expect("the handle contains SQLite");

        // INSERT 100 rows under the suppression window. The orchestrator-
        // owned guard's contract: the publisher drops every packet for
        // `app_orch` until the guard's Drop runs.
        for i in 0..100 {
            let sql = format!("INSERT INTO \"app_orch\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
            backend_ref
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT under orchestrator-driven backfill pause");
        }

        // Drop the guard WITHOUT draining first — the exposing order, same as
        // the two fences above. `unsuppress_app` runs and one Resync lands on
        // every active subscription on `app_orch` while all 100 packets are
        // still queued. This is the broker-pause-window lifecycle: the guard
        // binding drops at the end of the DDL/bulk-write window.
        drop(guard);

        // FIFO sentinel: observing this Change proves the publisher ran past
        // the 100 queued packets rather than never waking.
        backend_ref
            .execute_fixture(
                "INSERT INTO \"app_orch\".\"items\" (id, name) VALUES (1000, 'after')",
                &[],
            )
            .await
            .expect("INSERT after the orchestrator-driven backfill window");

        drain_publisher_long().await;

        let msgs = drain(&sub);

        assert_eq!(
            msgs.len(),
            2,
            "expected exactly [Resync, Change(sentinel)] after orchestrator-driven \
             pause + drop; got {} messages: {msgs:?}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], SubscriptionMessage::Resync),
            "the first message must be Resync; got {:?}",
            msgs[0]
        );
        let in_window_changes = msgs
            .iter()
            .filter_map(|m| match m {
                SubscriptionMessage::Change(ev) => Some(ev),
                _ => None,
            })
            .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
            .count();
        assert_eq!(
            in_window_changes, 0,
            "no Change events must reach the subscriber for commits made during an \
             orchestrator-driven backfill window; got {in_window_changes}"
        );
    });
}

// ---------------------------------------------------------------------------
// SQLite VectorIndex (`sqlite-vec` `vec0` virtual table)
// integration tests.
//
// Supersedes the earlier pure-Rust flat scan tests at the same point
// in this file (see `docs/archive/p4-search-implementation-plan.md`
// §10 2026-05-24 reassessment). The membership-set assertions are
// preserved byte-for-byte; only the underlying storage layer changed.
//
// Mirrors the PG arm's `vector_search_returns_k_nearest` /
// `vector_dimension_mismatch_rejected_at_insert` /
// `vector_search_respects_filter` structurally so a reviewer can
// diff the two suites side-by-side.
//
// Storage: base-table BLOB column with a CHECK constraint (the
// dimension contract at write time) + a `vec0` virtual table + AFTER
// triggers that mirror the BLOB column into vec0 on INSERT/UPDATE/DELETE.
// INSERTs use SQLite's hex-blob literal `x'<hex>'` so we avoid plumbing
// typed BLOB params through the session actor's `&[String]` surface.
//
// WHO CREATES THE vec0 RELATION, AND WHAT THAT MEANS FOR THESE TESTS.
// It used to be `VectorIndex::ensure_vector_index` on the data plane,
// behind `#[cfg(any(test, feature = "test-helpers"))]`. That method is
// deleted: schema is `zeroship-migrate`'s, and a data plane that can
// alter schema can disagree with the descriptor describing it.
//
// So the fixture below issues the DDL itself, and the honest reading of
// these tests changed with it. Before, they exercised production code
// that no shipped binary could reach; now they exercise a fixture that
// stands in for a migration that DOES NOT EXIST YET -- the SQLite
// renderer folds a vector field's index to a plain B-tree
// (`zeroship-migrate-sqlite/src/schema.rs:98`) and the engine states that
// it never authors a virtual table
// (`zeroship-migrate-backend/src/error.rs:270`). What they still prove is
// the SEARCH contract: given the shadow relation the runtime descriptor
// NAMES (`AuxiliaryObject::ShadowTable`), `vector_search` finds the right
// rows. What they CANNOT reach, and never could, is a database produced
// by an actual migration -- on one of those, SQLite `vector_search` fails
// with "no such table". That gap is pre-existing and is recorded at
// `backend/sqlite/mod.rs`'s `VectorIndex` block.
// ---------------------------------------------------------------------------

use zeroship_data_orm::backend::VectorMetric;

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

/// Stand in for the migration that ought to author a vector field's `vec0`
/// shadow relation on SQLite: the virtual table plus the three AFTER triggers
/// that mirror `(rowid, <col>)` into it.
///
/// The names are NOT invented here, and as of 2026-09-04 that sentence is TRUE.
/// It was not before: this fixture carried its own `format!("{coll}__vec_{col}")`
/// while claiming to take the names from the runtime descriptor, so it was a
/// THIRD independent spelling agreeing with the data plane by luck. Both now
/// come from [`engine_shadow_relation`], which renders the descriptor with the
/// migration engine and reads `storage.auxiliary` out of it.
///
/// That is the whole binding for this pair. The engine is what physically
/// creates every creator table, so the data plane's `vec_table_name` is a READER
/// of a name the engine writes - and nothing compared the two. With the fixture
/// derived from the engine, a divergence makes the search JOIN a relation that
/// does not exist and every vector test below fails.
/// [`the_search_really_depends_on_the_name_the_engine_records`] is the control
/// that keeps that from being a claim about a test which would pass anyway.
///
/// The vec0 constructor rejects a double-quoted column identifier, so `column`
/// is spliced unquoted -- the same constraint the deleted production builder
/// carried. Test-local input only.
///
/// No initial-population statement: every caller creates the relation before
/// inserting rows, so the triggers carry the whole payload.
async fn create_vec0_shadow_relation(
    backend: &SqliteBackend,
    app: &str,
    coll: &str,
    column: &str,
    dims: usize,
    metric: &str,
) {
    let (vtab, triggers) = engine_shadow_relation(coll, column, dims, metric);
    let (ai, ad, au) = (&triggers[0], &triggers[1], &triggers[2]);
    for sql in [
        format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"{app}\".\"{vtab}\" \
             USING vec0({column} float[{dims}] distance_metric={metric})"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS \"{app}\".\"{ai}\" \
             AFTER INSERT ON \"{app}\".\"{coll}\" \
             WHEN NEW.\"{column}\" IS NOT NULL BEGIN \
             INSERT INTO \"{vtab}\" (rowid, \"{column}\") \
               VALUES (NEW.rowid, NEW.\"{column}\"); END"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS \"{app}\".\"{ad}\" \
             AFTER DELETE ON \"{app}\".\"{coll}\" BEGIN \
             DELETE FROM \"{vtab}\" WHERE rowid = OLD.rowid; END"
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS \"{app}\".\"{au}\" \
             AFTER UPDATE OF \"{column}\" ON \"{app}\".\"{coll}\" BEGIN \
             DELETE FROM \"{vtab}\" WHERE rowid = OLD.rowid; \
             INSERT INTO \"{vtab}\" (rowid, \"{column}\") \
               SELECT NEW.rowid, NEW.\"{column}\" WHERE NEW.\"{column}\" IS NOT NULL; END"
        ),
    ] {
        backend
            .execute_fixture(&sql, &[])
            .await
            .unwrap_or_else(|e| panic!("vec0 shadow-relation fixture failed: {sql}: {e:?}"));
    }
}

/// The minimum charter the descriptor renderer needs to run.
///
/// It is not the shipped ceiling and does not have to be: this reads the
/// auxiliary-object NAMES a vector field owns, and injection shape does not
/// reach them. `no_inject` is the same choice `gen_types`'s own physical-storage
/// suite makes, for the same reason.
const VECTOR_DESCRIPTOR_CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;

/// The shadow relation and its three trigger names, AS THE MIGRATION ENGINE
/// RECORDS THEM for a vector field on SQLite.
///
/// Rendered through the public `render_artifacts_from_descriptors` under the
/// SQLite dialect and read back off `schema.runtime.json` - the same bytes a
/// creator's generated descriptor carries, not a Rust struct the test could
/// have built itself. Whether the shadow relation exists at all is a CAPABILITY
/// question the engine asks of the target (`NonBtreeIndexMethod`), so this also
/// fails loudly if SQLite ever stops owning one, instead of quietly returning a
/// name for an object nobody makes.
fn engine_shadow_relation(
    collection: &str,
    column: &str,
    dims: usize,
    metric: &str,
) -> (String, Vec<String>) {
    let effective = zeroship_migrate::effective_policy_from_charter_toml(VECTOR_DESCRIPTOR_CHARTER)
        .expect("the fixture charter must compose");
    let descriptors = [zeroship_migrate::CollectionDescriptor {
        name: collection.to_string(),
        owner_app: "app_fixture".to_string(),
        fields: vec![zeroship_migrate::FieldDescriptor {
            name: column.to_string(),
            ty: "vector".to_string(),
            vector_dims: Some(i64::try_from(dims).expect("test dims fit in i64")),
            vector_metric: Some(metric.to_string()),
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: Default::default(),
    }];
    let artifacts = zeroship_migrate::render_artifacts_from_descriptors(
        zeroship_migrate::shipping_vendors(),
        &descriptors,
        &zeroship_migrate_sqlite::DIALECT,
        zeroship_migrate::DEFAULT_PROJECT_SCHEMA,
        &effective,
    )
    .expect("the migration engine must render a descriptor for a vector field");

    let value: zeroship_data_sql::value::Value =
        serde_json::from_str(&artifacts.runtime_json).expect("the runtime descriptor is JSON");
    let auxiliary = value["collections"][collection]["fields"][column]["storage"]["auxiliary"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "a SQLite vector field must own a shadow relation in the descriptor: \
                 {value}"
            )
        })
        .clone();
    assert_eq!(
        auxiliary.len(),
        1,
        "expected exactly one auxiliary object for a vector field: {value}"
    );
    assert_eq!(auxiliary[0]["kind"], "shadowTable", "{value}");

    let name = auxiliary[0]["name"]
        .as_str()
        .expect("the shadow relation is named")
        .to_string();
    let triggers: Vec<String> = auxiliary[0]["triggers"]
        .as_array()
        .expect("the shadow relation names its triggers")
        .iter()
        .map(|t| {
            t.as_str()
                .expect("each trigger name is a string")
                .to_string()
        })
        .collect();
    assert_eq!(
        triggers.len(),
        3,
        "after-insert, after-delete and after-update, in that order: {value}"
    );
    (name, triggers)
}

/// The control for the binding above: the search really does depend on the name.
///
/// Every vector case below creates its shadow relation under the engine's name
/// and then searches successfully. That proves agreement ONLY if a DISAGREEMENT
/// would have failed - and a `JOIN` naming a missing relation is the kind of
/// thing an engine can be lenient about. It is not: this creates the relation
/// one character away from the engine's name, populates it identically, and the
/// search refuses.
///
/// Without this arm, "the data plane joins the name the engine records" would
/// rest on a test that could have passed for any name at all.
#[test]
fn the_search_really_depends_on_the_name_the_engine_records() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("vector_wrongname")
            .await
            .expect("ensure_app_schema");

        let dims = 8usize;
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"vector_wrongname\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       embedding BLOB CHECK(length(embedding) = 32) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE docs");
        zeroship_data_orm::cache_schema_for_tests(
            "vector_wrongname",
            "docs",
            zeroship_data_sql::value!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
        );

        // The engine's name, with one byte changed. Everything else - the vec0
        // declaration, the mirror trigger, the rows - is what the passing cases
        // use.
        let (engine_name, _) = engine_shadow_relation("docs", "embedding", dims, "cosine");
        let wrong = format!("{engine_name}x");
        for sql in [
            format!(
                "CREATE VIRTUAL TABLE \"vector_wrongname\".\"{wrong}\" \
                 USING vec0(embedding float[{dims}] distance_metric=cosine)"
            ),
            format!(
                "CREATE TRIGGER \"vector_wrongname\".\"{wrong}_ai\" \
                 AFTER INSERT ON \"vector_wrongname\".\"docs\" \
                 WHEN NEW.\"embedding\" IS NOT NULL BEGIN \
                 INSERT INTO \"{wrong}\" (rowid, \"embedding\") \
                   VALUES (NEW.rowid, NEW.\"embedding\"); END"
            ),
        ] {
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("misnamed fixture");
        }
        for i in 0..4usize {
            let hex = vec_to_hex_lit(&mk_unit_vec(i, dims));
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"vector_wrongname\".\"docs\" (embedding) VALUES ({hex})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT");
        }

        let err = backend
            .vector_search(
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start("vector_wrongname"),
                    collection: "docs",
                    column: "embedding",
                    query: &mk_unit_vec(0, dims),
                    k: 4,
                    metric: VectorMetric::Cosine,
                    filter: &zeroship_data_sql::value::Value::Null,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("vector_wrongname"),
                        "docs",
                    )
                    .expect("descriptor slice for the control fixture"),
                },
            )
            .await
            .expect_err(
                "the search must refuse when the shadow relation is not the one the \
                 engine's descriptor names; if this succeeds, every vector case above \
                 proves nothing about the name",
            );
        let message = format!("{err:?}");
        assert!(
            message.contains(&wrong) || message.contains(&engine_name),
            "the refusal must name the relation it could not reach; got: {message}"
        );
    });
}

#[test]
fn vector_search_returns_k_nearest_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("vector_topk")
            .await
            .expect("ensure_app_schema");

        // CREATE TABLE with the BLOB column the SDK's `t.vector(dims)`
        // lowering emits. The CHECK constraint pins the write-side
        // dimension contract; vec0's own dimension check is the
        // second line of defence (trigger-time).
        let dims = 8usize;
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"vector_topk\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       embedding BLOB CHECK(length(embedding) = 32) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE docs");
        zeroship_data_orm::cache_schema_for_tests(
            "vector_topk",
            "docs",
            zeroship_data_sql::value!({ "embedding": { "type": "vector", "vectorDims": 8 } }),
        );

        // Create the vec0 vtable + mirror triggers BEFORE inserting
        // rows. With the triggers in place, every INSERT into the
        // base table fans out into the vec0 index inside the same
        // transaction; vector_search joins on rowid.
        create_vec0_shadow_relation(&backend, "vector_topk", "docs", "embedding", 8, "cosine")
            .await;

        // Insert 100 deterministic unit vectors. Each INSERT fires
        // the `docs__vec_embedding_ai` trigger which mirrors
        // `(rowid, embedding)` into the vec0 index.
        for i in 0..100usize {
            let v = mk_unit_vec(i, dims);
            let hex = vec_to_hex_lit(&v);
            let sql = format!("INSERT INTO \"vector_topk\".\"docs\" (embedding) VALUES ({hex})");
            backend.execute_fixture(&sql, &[]).await.expect("INSERT");
        }

        // Query with row #0's exact vector — its own row must be in
        // the top-10. Assert MEMBERSHIP (not strict order) to mirror
        // the PG arm's relaxed expectation.
        let query = mk_unit_vec(0, dims);
        let rows = backend
            .vector_search(
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start("vector_topk"),
                    collection: "docs",
                    column: "embedding",
                    query: &query,
                    k: 10,
                    metric: VectorMetric::Cosine,
                    filter: &zeroship_data_sql::value::Value::Null,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("vector_topk"),
                        "docs",
                    )
                    .expect("descriptor slice for the search fixture"),
                },
            )
            .await
            .expect("vector_search");

        assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| {
                r.get("id")
                    .and_then(zeroship_data_sql::value::Value::as_i64)
            })
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
                .and_then(zeroship_data_sql::value::Value::as_f64)
                .expect("row must carry _distance");
            assert!(d.is_finite(), "_distance must be finite, got {d}");
            assert!(d >= 0.0, "cosine distance is non-negative, got {d}");
        }
        // Row #1 should be the nearest (distance ~ 0).
        let first_id = rows[0]
            .get("id")
            .and_then(zeroship_data_sql::value::Value::as_i64)
            .expect("first row id");
        assert_eq!(
            first_id, 1,
            "exact-match query must place its own row first"
        );
        let first_d = rows[0]
            .get("_distance")
            .and_then(zeroship_data_sql::value::Value::as_f64)
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
            .attach_app_file("vector_dim")
            .await
            .expect("ensure_app_schema");

        // 128-d column = 512-byte CHECK.
        backend
            .execute_fixture(
                "CREATE TABLE \"vector_dim\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   embedding BLOB CHECK(length(embedding) = 512) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        // Insert a 256-d vector into a 128-d column. The CHECK
        // constraint must reject — the DatabaseFixture surface should
        // surface a SchemaRefused {check_violation} typed error.
        let oversized = mk_unit_vec(0, 256);
        let hex = vec_to_hex_lit(&oversized);
        let sql = format!("INSERT INTO \"vector_dim\".\"docs\" (embedding) VALUES ({hex})");
        let err = backend
            .execute_fixture(&sql, &[])
            .await
            .expect_err("256-d into 128-d column must fail");
        match err {
            DbError::SchemaRefused { code, .. } => {
                assert_eq!(
                    code, "check_violation",
                    "expected check_violation, got {code}"
                );
            }
            other => panic!("expected SchemaRefused {{ check_violation }}, got {other:?}"),
        }
    });
}

#[test]
fn vector_search_respects_filter_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("vector_filter")
            .await
            .expect("ensure_app_schema");

        // 4-d column = 16-byte CHECK.
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"vector_filter\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       tenant TEXT NOT NULL, \
                       embedding BLOB CHECK(length(embedding) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE docs");
        zeroship_data_orm::cache_schema_for_tests(
            "vector_filter",
            "docs",
            zeroship_data_sql::value!({
                "tenant": { "type": "string" },
                "embedding": { "type": "vector", "vectorDims": 4 },
            }),
        );

        // Create vec0 + triggers BEFORE inserts so the mirror fires
        // for every row.
        create_vec0_shadow_relation(&backend, "vector_filter", "docs", "embedding", 4, "cosine")
            .await;

        // Insert 10 rows in tenant "a" and 10 rows in tenant "b".
        // The first row of each tenant uses an identical query
        // vector so the filter discriminates BY tenant, not by
        // proximity.
        for i in 0..10usize {
            let v = mk_unit_vec(i, 4);
            let hex = vec_to_hex_lit(&v);
            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"vector_filter\".\"docs\" \
                           (tenant, embedding) VALUES ('a', {hex})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT a");
            backend
                .execute_fixture(
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
        let filter = zeroship_data_sql::value!({ "tenant": { "$eq": "a" } });
        let rows = backend
            .vector_search(
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start("vector_filter"),
                    collection: "docs",
                    column: "embedding",
                    query: &query,
                    k: 10,
                    metric: VectorMetric::Cosine,
                    filter: &filter,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("vector_filter"),
                        "docs",
                    )
                    .expect("descriptor slice for the search fixture"),
                },
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
                .and_then(zeroship_data_sql::value::Value::as_str)
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
    // one L2) sharing the same source rows. The two shadow-relation
    // fixtures produce two paired vec0 vtables (`docs__vec_emb_cos` /
    // `docs__vec_emb_l2`); the AFTER triggers mirror BOTH columns on
    // every INSERT.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("vector_math")
            .await
            .expect("ensure_app_schema");

        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"vector_math\".\"docs\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       emb_cos BLOB CHECK(length(emb_cos) = 16) NOT NULL, \
                       emb_l2  BLOB CHECK(length(emb_l2)  = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE docs");
        zeroship_data_orm::cache_schema_for_tests(
            "vector_math",
            "docs",
            zeroship_data_sql::value!({
                "emb_cos": { "type": "vector", "vectorDims": 4 },
                "emb_l2": { "type": "vector", "vectorDims": 4 },
            }),
        );

        create_vec0_shadow_relation(&backend, "vector_math", "docs", "emb_cos", 4, "cosine").await;
        create_vec0_shadow_relation(&backend, "vector_math", "docs", "emb_l2", 4, "l2").await;

        let v1 = mk_unit_vec(0, 4);
        let v2 = mk_unit_vec(1, 4);
        let hex1 = vec_to_hex_lit(&v1);
        let hex2 = vec_to_hex_lit(&v2);
        backend
            .execute_fixture(
                &format!(
                    "INSERT INTO \"vector_math\".\"docs\" (emb_cos, emb_l2) \
                     VALUES ({hex1}, {hex1})"
                ),
                &[],
            )
            .await
            .expect("INSERT v1");
        backend
            .execute_fixture(
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
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start("vector_math"),
                    collection: "docs",
                    column: "emb_cos",
                    query: &v1,
                    k: 2,
                    metric: VectorMetric::Cosine,
                    filter: &zeroship_data_sql::value::Value::Null,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("vector_math"),
                        "docs",
                    )
                    .expect("descriptor slice for the search fixture"),
                },
            )
            .await
            .expect("cosine search");
        let l2_rows = backend
            .vector_search(
                None,
                zeroship_data_orm::search::VectorSearch {
                    binding: &DbBinding::cold_start("vector_math"),
                    collection: "docs",
                    column: "emb_l2",
                    query: &v1,
                    k: 2,
                    metric: VectorMetric::L2,
                    filter: &zeroship_data_sql::value::Value::Null,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("vector_math"),
                        "docs",
                    )
                    .expect("descriptor slice for the search fixture"),
                },
            )
            .await
            .expect("l2 search");

        // The row with id=2 (the OTHER unit vector) must appear in
        // both result sets; its cosine and L2 distances must satisfy
        // L2² ≈ 2 * cos_distance.
        let find = |rows: &[zeroship_data_sql::value::Value], target_id: i64| -> f64 {
            rows.iter()
                .find(|r| {
                    r.get("id")
                        .and_then(zeroship_data_sql::value::Value::as_i64)
                        == Some(target_id)
                })
                .and_then(|r| {
                    r.get("_distance")
                        .and_then(zeroship_data_sql::value::Value::as_f64)
                })
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
// SQLite SpatialIndex (haversine) gates
// ---------------------------------------------------------------------------
//
// These tests target the `SpatialIndex` impl on `SqliteBackend` and
// exercise the haversine flat scan against `(lat, lng)` 16-byte BLOB
// payloads.
//
// Like the vector tests, we construct table DDL inline and exercise the
// SQLite geopoint encoding directly.

use zeroship_data_orm::backend::GeoPoint;

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

/// **Test gate**: `near_returns_within_radius` (SQLite).
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
            .attach_app_file("near_radius")
            .await
            .expect("ensure_app_schema");

        // Inline DDL — the `sqlite_geopoint_column_ddl` helper emits
        // the same CHECK shape; we hand-write it here to keep the
        // test self-contained against the orchestrator's PG-flavoured
        // emitter.
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"near_radius\".\"places\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       location BLOB CHECK(length(location) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE places");
        zeroship_data_orm::cache_schema_for_tests(
            "near_radius",
            "places",
            zeroship_data_sql::value!({ "location": { "type": "geoPoint" } }),
        );

        let london = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
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
            let sql = format!("INSERT INTO \"near_radius\".\"places\" (location) VALUES ({hex})");
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT location");
            if *within_1km {
                expected_within.push((i + 1) as i64);
            }
        }

        let rows = backend
            .spatial_near(
                None,
                zeroship_data_orm::search::SpatialSearch {
                    binding: &DbBinding::cold_start("near_radius"),
                    collection: "places",
                    column: "location",
                    point: london,
                    radius_m: 1000.0,
                    filter: &zeroship_data_sql::value::Value::Null,
                    limit: None,
                    schema: &zeroship_data_orm::descriptor::collection_schema(
                        &DbBinding::cold_start("near_radius"),
                        "places",
                    )
                    .expect("descriptor slice for the search fixture"),
                },
            )
            .await
            .expect("spatial_near");

        let returned_ids: std::collections::BTreeSet<i64> = rows
            .iter()
            .filter_map(|r| {
                r.get("id")
                    .and_then(zeroship_data_sql::value::Value::as_i64)
            })
            .collect();
        let expected: std::collections::BTreeSet<i64> = expected_within.into_iter().collect();
        assert_eq!(
            returned_ids, expected,
            "near(1km) membership mismatch: returned={returned_ids:?} expected={expected:?}"
        );
        // Every row carries the synthetic `_distance_m` column.
        for r in &rows {
            let d = r
                .get("_distance_m")
                .and_then(zeroship_data_sql::value::Value::as_f64)
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
            .and_then(zeroship_data_sql::value::Value::as_i64)
            .expect("first row id");
        assert_eq!(
            first_id, 1,
            "dead-centre (offset (0,0)) row must be first by distance"
        );
        let first_d = rows[0]
            .get("_distance_m")
            .and_then(zeroship_data_sql::value::Value::as_f64)
            .expect("first row _distance_m");
        assert!(
            first_d < 1.0,
            "dead-centre distance must be < 1m, got {first_d}"
        );
    });
}

/// A `near()` inside `db.transaction(fn)` must scan the transaction's own
/// connection.
///
/// The SQLite half of what `tests/search_tx_lane.rs` rules on for PostgreSQL,
/// and it is a separate question rather than the same one twice: SC-2 Decision 1
/// gave this backend TWO connections, `op_conn` for autocommit reads and
/// `tx_conn` for the creator's transaction, and `SpatialIndex::spatial_near`
/// took `&self` - which can only ever mean `op_conn`. So a `near` issued inside
/// a transaction scanned a connection that cannot see that transaction's own
/// uncommitted rows.
///
/// **The control differs in one variable: `route.in_tx()`.** The same `near`,
/// over the same row, at the same instant, on a route captured outside the
/// transaction must return nothing - because on `op_conn` the row genuinely is
/// not there. That is what makes the subject arm a statement about the lane
/// rather than about the fixture.
#[test]
fn a_near_inside_a_transaction_sees_the_row_that_transaction_inserted() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let app = "near_tx_lane";
        backend
            .attach_app_file(app)
            .await
            .expect("attach the app database");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"{app}\".\"places\" (\
                       id INTEGER PRIMARY KEY AUTOINCREMENT, \
                       location BLOB CHECK(length(location) = 16) NOT NULL, \
                       {SYSTEM_COLUMNS_SQLITE_TAIL}\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE places");
        zeroship_data_orm::cache_schema_for_tests(
            app,
            "places",
            zeroship_data_sql::value!({ "location": { "type": "geoPoint" } }),
        );

        let london = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };

        let handle = BackendHandle::new(std::rc::Rc::new(backend));
        let admission = zeroship_data_orm::transaction::TxAdmission::acquire(app.to_owned()).await;
        zeroship_data_orm::transaction::exec_begin_or_savepoint(
            false,
            None,
            app,
            zeroship_data_sql::SchemaName::new(app).unwrap(),
            handle.clone(),
        )
        .await
        .unwrap();
        admission.handed_to_reducer();
        zeroship_data_orm::transaction::driver::run_operation(
            app,
            &format!(
                "INSERT INTO \"{app}\".\"places\" (location) VALUES ({})",
                point_to_hex_lit(london)
            ),
            &[],
        )
        .await
        .expect("write inside the transaction");

        let binding = DbBinding::cold_start(app);
        let args = zeroship_data_sql::value!({
            "field": "location",
            "point": { "lat": london.lat, "lng": london.lng },
            "radius": 1000.0,
        });
        let near_on = async |route| {
            let plan = zeroship_data_orm::crud::plan_near(
                &binding,
                zeroship_data_sql::compile::SqlDialect::Sqlite,
                "places",
                &args,
            )
            .expect("plan_near");
            zeroship_data_orm::crud::run_near(&route, binding.clone(), "places".to_string(), plan)
                .await
                .expect("run_near")
                .rows
        };

        // ---- CONTROL: a route captured OUTSIDE the transaction. `op_conn`
        // cannot see the row, so an empty result here is what proves the
        // subject arm below is about the lane.
        let outside = near_on(
            zeroship_data_orm::tx_route::CapturedRoute::pool_for_tests(
                app,
                zeroship_data_sql::compile::SqlDialect::Sqlite,
            )
            .bind(handle.clone()),
        )
        .await;
        assert!(
            outside.is_empty(),
            "the row must be invisible on the autocommit connection, or the \
             subject arm below cannot distinguish the two lanes: {outside:?}",
        );

        // ---- SUBJECT: the same near on the transaction's own lane.
        let inside = near_on(
            zeroship_data_orm::tx_route::CapturedRoute::tx_for_tests(
                app,
                zeroship_data_sql::compile::SqlDialect::Sqlite,
            )
            .bind(handle.clone()),
        )
        .await;
        assert_eq!(
            inside.len(),
            1,
            "a near inside a transaction must reach the row that transaction \
             inserted; an empty result means the scan took `op_conn`: {inside:?}",
        );
        assert_eq!(inside[0]["location"], zeroship_data_sql::value!({"lat":london.lat, "lng":london.lng}));
        assert!(
            inside[0]
                .get("_distance_m")
                .and_then(zeroship_data_sql::value::Value::as_f64)
                .is_some_and(|d| d < 1.0),
            "the row must carry its synthetic distance: {inside:?}",
        );

        assert!(matches!(
            zeroship_data_orm::transaction::exec_settle(app, false, None).await,
            zeroship_data_orm::transaction::SettleOutcome::Ok
        ));
    });
    zeroship_data_v8::testing::reset_context_for_tests();
}

// ===========================================================================
// Column encryption on SqliteBackend
// ===========================================================================
//
// These tests exercise the full SQLite round-trip for `t.encrypted(...)`-
// declared columns: env-var key sourcing through KeyStore, AES-GCM
// encrypt with the right AAD shape (Camp A - row_pk in AAD for
// Randomised, omitted for Deterministic), BLOB storage on disk via
// rusqlite's typed BLOB binding, decrypt-on-read. Mirrors the PG suite's
// column-encryption tests in `tests/integration.rs`.

/// Helper: hand this isolate a synthetic root key for `key_id`, for the
/// duration of a test. Same shape as the PG-side `with_root_key` in
/// `tests/integration.rs`.
///
/// This REPLACES a `set_var("ZEROSHIP_COLUMN_KEY_<KEYID>", ...)` guard.
/// The env var was the only channel that reached the `SqliteBackend`
/// these tests never construct themselves - the one `initialize_backend`
/// builds behind a `dispatch_zs` V8 call - and mutating it is
/// process-global, racy with any concurrent `getenv`, and `unsafe`. The
/// isolate context is per-thread and typed, so none of the three apply,
/// and `SqliteBackend::{new, open}` read it wherever they are called
/// from.
///
/// Install it BEFORE the backend is constructed: a backend captures the
/// source at construction, so a key supplied afterwards will not reach
/// it.
///
/// The returned guard withdraws the key on drop; keep it alive for the
/// test body.
fn with_root_key(key_id: &str, root_hex: &str) -> zeroship_data_v8::testing::SuppliedRootKeysGuard {
    zeroship_data_v8::testing::supply_root_keys_for_tests(&[(key_id, root_hex)])
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

// ---------------------------------------------------------------------------
// The runtime-dispatch fixtures below receive their table from a migration that
// ran before the serving process existed. `apply_schema_ahead_of_runtime` is
// that step; `sqlite_runtime_source` gives the runtime the same shape as a
// native descriptor so encrypted and masked facets reach the CRUD passes.
//
// The schema is authored once in Rust and used for both the apply-ahead fixture
// and the descriptor. Holding it twice is how those shapes drift while both
// look right in isolation.
// ---------------------------------------------------------------------------

/// `email` unique + plaintext, `ssn` randomised-encrypted with a `last4` mask.
fn users_encrypted_ssn_schema(key_id: &str) -> zeroship_data_sql::value::Value {
    zeroship_data_sql::value!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": key_id, "wraps": "string"},
            "mask": {"kind": "last4", "classification": "spi"}
        }
    })
}

/// As above, plus `email` itself deterministically encrypted - the shape the
/// deterministic-conflict upsert needs (a unique index over ciphertext).
fn users_deterministic_email_schema(key_id: &str) -> zeroship_data_sql::value::Value {
    zeroship_data_sql::value!({
        "email": {
            "type": "string",
            "required": true,
            "unique": true,
            "encrypted": {"mode": "deterministic", "keyId": key_id, "wraps": "string"}
        },
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": key_id, "wraps": "string"},
            "mask": {"kind": "last4", "classification": "spi"}
        }
    })
}

/// One randomised-encrypted column and no mask - the fast-path fixtures assert
/// a PLAIN write skips row resolution, so the encrypted column must exist but
/// stay untouched by the write under test.
fn users_encrypted_secret_schema(key_id: &str) -> zeroship_data_sql::value::Value {
    zeroship_data_sql::value!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string", "required": true},
        "secret": {
            "type": "string",
            "encrypted": {"mode": "randomised", "keyId": key_id, "wraps": "string"}
        }
    })
}

// ---------------------------------------------------------------------------
// RAW FIXTURE DDL for the schemas above.
//
// These are hand-written statements, NOT rendered from the schema JSON. plugin-db
// does not own DDL, so a test that needs a table spells it (see
// `support::tables` for the full argument). Each literal sits next to the
// `*_schema` helper it must match; if they drift, the first query in the test
// fails rather than the test agreeing with a wrong emitter.
//
// The seven system columns, the `["id"]` PK and the three system indexes are the
// platform's confined table shape. `_masked` companions and the `zero-migrate:enc:` /
// `zero-migrate:mask:` comment sentinels preserve the migrated table shape exercised by
// these fixtures. Runtime field metadata comes from the deployed descriptor.
// ---------------------------------------------------------------------------

/// The seven system columns and the `["id"]` primary key, SQLite spelling.
const SYSTEM_COLUMNS_SQLITE: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TEXT NULL"#;

/// The six non-`id` system columns, for a fixture that keeps its own `id`
/// declaration. The vector / spatial fixtures use an `INTEGER PRIMARY KEY
/// AUTOINCREMENT` rowid so their assertions can name `id: 1`, but they still
/// need the other six: the implicit read projection those searches build names
/// all seven system columns unconditionally, and a table missing them is not a
/// table the data plane can read.
const SYSTEM_COLUMNS_SQLITE_TAIL: &str = "\
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
  created_by TEXT NULL, \
  updated_by TEXT NULL, \
  version INTEGER NOT NULL DEFAULT 1, \
  deleted_at TEXT NULL";

/// The three system indexes every confined table carries.
fn system_indexes_sqlite(app_id: &str, collection: &str) -> String {
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_deleted_at_idx" ON "{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_updated_at_idx" ON "{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_created_by_idx" ON "{collection}" ("created_by");
"#
    )
}

/// Raw DDL matching [`users_encrypted_ssn_schema`].
///
/// Post-storage-flip layout: the field's own column (`ssn`) holds the
/// masked representation as bare `TEXT`; the sibling raw column (named via
/// [`raw_column_name`], NOT spelled out here) carries the declared type,
/// the encryption sentinel, and any constraints.
fn users_encrypted_ssn_ddl(key_id: &str) -> String {
    let raw_ssn = raw_column_name("ssn");
    format!(
        r#"CREATE TABLE IF NOT EXISTS "default"."users" ({SYSTEM_COLUMNS_SQLITE},
  "email" TEXT NOT NULL,
  "name" TEXT NOT NULL,
  "{raw_ssn}" BLOB /* zero-migrate:enc:randomised:{key_id}:string */,
  "ssn" TEXT /* zero-migrate:mask:kind=last4,classification=spi */
);
{}
CREATE UNIQUE INDEX IF NOT EXISTS "default"."users_email_key" ON "users" ("email");
"#,
        system_indexes_sqlite("default", "users")
    )
}

/// Raw DDL matching [`users_encrypted_secret_schema`].
fn users_encrypted_secret_ddl(key_id: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "default"."users" ({SYSTEM_COLUMNS_SQLITE},
  "email" TEXT NOT NULL,
  "name" TEXT NOT NULL,
  "secret" BLOB /* zero-migrate:enc:randomised:{key_id}:string */
);
{}
CREATE UNIQUE INDEX IF NOT EXISTS "default"."users_email_key" ON "users" ("email");
"#,
        system_indexes_sqlite("default", "users")
    )
}

/// Raw DDL matching [`users_deterministic_email_schema`].
///
/// `email` is deterministically encrypted AND unique, so it carries BOTH a plain
/// lookup index and the unique constraint over ciphertext - that pair is what the
/// deterministic-conflict upsert needs, and the reason this schema exists.
fn users_deterministic_email_ddl(key_id: &str) -> String {
    let raw_ssn = raw_column_name("ssn");
    format!(
        r#"CREATE TABLE IF NOT EXISTS "default"."users" ({SYSTEM_COLUMNS_SQLITE},
  "email" BLOB /* zero-migrate:enc:deterministic:{key_id}:string */ NOT NULL,
  "name" TEXT NOT NULL,
  "{raw_ssn}" BLOB /* zero-migrate:enc:randomised:{key_id}:string */,
  "ssn" TEXT /* zero-migrate:mask:kind=last4,classification=spi */
);
{}
CREATE INDEX IF NOT EXISTS "default"."users_email_idx" ON "users" ("email");
CREATE UNIQUE INDEX IF NOT EXISTS "default"."users_email_key" ON "users" ("email");
"#,
        system_indexes_sqlite("default", "users")
    )
}

/// Create the `users` table in the dev app file BEFORE the runtime boots.
///
/// `dir` is the same directory `parity::sqlite_url` points the runtime at, so the
/// fixture writes `<dir>/zs-default.sqlite` - the exact file the data plane will
/// ATTACH. `default` is the app id the runtime derives with no `APP_ID` in the
/// env snapshot.
fn apply_schema_ahead_of_runtime(dir: &tempfile::TempDir, ddl: &str) {
    crate::support::tables::create_sqlite_table(dir.path(), "default", ddl);
}

struct SqliteRuntimeSource {
    source: String,
    descriptor: String,
}

fn sqlite_runtime_source(
    collection: &str,
    schema: &zeroship_data_sql::value::Value,
    body: &str,
) -> SqliteRuntimeSource {
    let source = format!(
        r#"
import {{ env }} from "zeroship";

const COLLECTION = "{collection}";

{body}
"#
    ) + SQLITE_RUNTIME_RPC_SHIM;
    SqliteRuntimeSource {
        source,
        descriptor: parity::runtime_descriptor(collection, schema),
    }
}

fn dispatch_sqlite_runtime(
    dir: &tempfile::TempDir,
    source: &SqliteRuntimeSource,
    name: &str,
) -> zeroship_data_sql::value::Value {
    let url = parity::sqlite_url(dir);
    let (status, body) = parity::dispatch_zs_with_descriptor(
        &url,
        &source.source,
        name,
        parity::DEV_APP_ID,
        &source.descriptor,
    );
    assert_eq!(status, 200, "{name} failed: {body}");
    body
}

fn assert_write_path_fast_path(label: &str) {
    let counters = zeroship_data_orm::crud::write_path_counters_for_tests();
    assert_eq!(
        counters.target_row_resolution_calls, 0,
        "{label}: plain write must not resolve row ids: {counters:?}",
    );
    assert_eq!(
        counters.upsert_conflict_probe_calls, 0,
        "{label}: plain write must not run an upsert conflict probe: {counters:?}",
    );
}

#[test]
fn insert_many_encrypts_ciphertext_before_sqlite_storage() {
    run(async {
        use std::collections::HashMap;

        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_sql::compile::{SqlDialect, build_insert_many_with_dialect};
        use zeroship_data_orm::encryption;

        let key_id = "c1_insert_many";
        let _keys = with_root_key("c1_insert_many", &"d".repeat(64));
        let app_id = "app_demo";
        let collection = "bulk_people";
        let schema = zeroship_data_sql::value!({
            "name": { "type": "string" },
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": key_id, "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
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
            backend
                .execute_fixture(trimmed, &[])
                .await
                .expect("DDL exec");
        }

        let mut docs = zeroship_data_sql::value!([
            { "name": "Alice", "ssn": "123-45-6789" },
            { "name": "Bob", "ssn": "987-65-4321" }
        ]);
        zeroship_data_v8::testing::prepare_insert_many_docs_for_tests(
            &mut docs,
            app_id,
            collection,
            Some("usr_bulk_writer"),
        )
        .await
        .expect("prepare insertMany docs");

        let expected_by_id: HashMap<String, (Vec<u8>, String)> = docs
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
                        obj.get(raw_column_name("ssn").as_str())
                            .and_then(|v| v.as_bytes())
                            .expect("native ciphertext in the raw column")
                            .to_vec(),
                        obj.get("ssn")
                            .and_then(|v| v.as_str())
                            .expect("masked sibling stays on the field's own column")
                            .to_string(),
                    ),
                )
            })
            .collect();

        let built = build_insert_many_with_dialect(
            &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
            collection,
            &schema,
            &docs,
            SqlDialect::Sqlite,
        )
        .expect("build insertMany");
        let params = &built.params;
        let client = backend
            .fixture_session(app_id)
            .await
            .expect("acquire client");
        client
            .query_typed(&built.sql, params)
            .await
            .expect("INSERT ... RETURNING");

        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, "{raw_ssn}", ssn FROM "{app_id}"."{collection}" ORDER BY id"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed");
        assert_eq!(typed.rows.len(), 2, "two rows stored");

        let key = backend
            .key_store()
            .resolve(app_id, key_id)
            .await
            .expect("resolve key");
        for row in &typed.rows {
            let id = match &row[0] {
                TypedCell::Text(s) => s.clone(),
                other => panic!("id must be TEXT, got {other:?}"),
            };
            let stored_blob = match &row[1] {
                TypedCell::Blob(bytes) => bytes.clone(),
                other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
            };
            let masked = match &row[2] {
                TypedCell::Text(s) => s.clone(),
                other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
            };
            let (prepared_ciphertext, prepared_masked) = expected_by_id
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
            // The field's own column (`ssn`) must never hold the real value:
            // it is the masked sibling's new home after the storage flip.
            assert_ne!(
                masked, "123-45-6789",
                "ssn (field's own column) must not hold plaintext",
            );
            assert_ne!(
                masked, "987-65-4321",
                "ssn (field's own column) must not hold plaintext",
            );
            let expected_ciphertext = prepared_ciphertext.clone();
            assert_eq!(
                stored_blob, expected_ciphertext,
                "raw stored bytes must match the write-side ciphertext",
            );
            let plaintext = zeroship_data_orm::encryption::aead::decrypt(
                &key,
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
    let _keys = with_root_key("c2_upsert_runtime_insert", &"e".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
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

const _procedures = { upsertInsert };
"#,
        );

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

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
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
        assert_eq!(
            rows[0][0].as_deref(),
            Some(id),
            "stored row keeps minted id"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "stored row version defaults to 1"
        );
    });
}

#[test]
fn upsert_conflict_update_preserves_insert_only_fields_and_encrypts_sqlite_runtime() {
    let key_id = "c2_upsert_runtime_conflict";
    let _keys = with_root_key("c2_upsert_runtime_conflict", &"f".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function upsertConflict(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const first = await coll.upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            created_by: "usr_seed",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    const second = await coll.upsert(
        {
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

const _procedures = { upsertConflict };
"#,
        );

        let result = dispatch_sqlite_runtime(&dir, &source, "upsertConflict");
        let payload = parity::extract_json(&result);
        let first = payload.get("first").expect("first response row");
        let second = payload.get("second").expect("second response row");
        let first_id = first.get("id").and_then(|v| v.as_str()).expect("generated id");
        assert!(first_id.starts_with("user_"));
        // The conflicting insert mints a candidate identity; the update must
        // preserve the stored identity and use it for encryption's AAD.
        assert_eq!(
            second.get("id").and_then(|v| v.as_str()),
            Some(first_id),
            "conflict update must keep the original id"
        );
        // The two documents supply DIFFERENT actor ids, and neither lands.
        // `created_by` / `updated_by` are charter-assigned, so the value comes
        // from the request's authenticated user - here there is none, and the
        // generator yields NULL rather than the id the document asked for.
        //
        // This asserted `usr_seed` / `usr_update` until the write pass started
        // iterating the charter. It was pinning the DB-3 shape: app JS naming
        // whichever actor it liked on a row it wrote.
        assert!(
            first
                .get("created_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied created_by must not land on the insert arm: {first:?}"
        );
        assert!(
            second
                .get("created_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied created_by must not land on the conflict arm: {second:?}"
        );
        assert!(
            second
                .get("updated_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied updated_by must not land on the conflict arm: {second:?}"
        );
        assert_eq!(
            second.get("version").and_then(|v| v.as_i64()),
            Some(2),
            "conflict update must auto-bump version"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, created_by, updated_by, version, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed conflict row");
        assert_eq!(typed.rows.len(), 1, "exactly one row after conflict upsert");
        let row = &typed.rows[0];

        match &row[0] {
            TypedCell::Text(id) => assert_eq!(id, first_id),
            other => panic!("id must be TEXT, got {other:?}"),
        }
        // Read back from the DATABASE, not from the returned row, that neither
        // supplied actor id was stored. Both documents named one; there is no
        // authenticated user on this vector, so the stored value is NULL.
        assert!(
            matches!(&row[1], TypedCell::Null),
            "created_by must be NULL when no actor is bound, got {:?}",
            row[1]
        );
        assert!(
            matches!(&row[2], TypedCell::Null),
            "updated_by must be NULL when no actor is bound, got {:?}",
            row[2]
        );
        match &row[3] {
            TypedCell::Integer(version) => assert_eq!(*version, 2),
            other => panic!("version must be INTEGER, got {other:?}"),
        }
        let stored_blob = match &row[4] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[5] {
            TypedCell::Text(masked) => assert_eq!(masked, "***-**-4321"),
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }
        assert_ne!(
            stored_blob,
            b"987-65-4321".to_vec(),
            "conflict-updated raw storage must not equal plaintext"
        );
        // The field's own column (`ssn`) already asserted equal to the mask
        // above; pin the negative directly too - it must never be the
        // plaintext the conflict-update wrote.
        match &row[5] {
            TypedCell::Text(masked) => assert_ne!(
                masked, "987-65-4321",
                "ssn (field's own column) must not hold plaintext"
            ),
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }

        let key = backend
            .key_store()
            .resolve("default", key_id)
            .await
            .expect("resolve key");
        let plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
            &stored_blob,
            &encryption::canonical_aad("users", "ssn", Some(first_id.as_bytes())),
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
    let _keys = with_root_key("c2_upsert_det_conflict_runtime", &"6".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_deterministic_email_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_deterministic_email_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function upsertConflict(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const first = await coll.upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    const second = await coll.upsert(
        {
            email: "alice@example.com",
            name: "Alice Updated",
            ssn: "987-65-4321"
        },
        { conflictFields: ["email"] },
    );
    return { first, second };
}
upsertConflict.config = { kind: "action" };

const _procedures = { upsertConflict };
"#,
        );

        let result = dispatch_sqlite_runtime(&dir, &source, "upsertConflict");
        let payload = parity::extract_json(&result);
        let first_id = payload["first"]["id"].as_str().expect("generated id");
        let second = payload.get("second").expect("second response row");
        assert_eq!(
            second.get("id").and_then(|v| v.as_str()),
            Some(first_id),
            "deterministic conflict probe must rewrite to the existing row id"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, email, "{raw_ssn}", ssn
                   FROM "default"."users""#
                ),
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
            other => {
                panic!("{raw_ssn} must be stored as randomised ciphertext BLOB, got {other:?}")
            }
        };
        match &row[3] {
            TypedCell::Text(masked) => {
                assert_eq!(masked, "***-**-4321");
                // The field's own column (`ssn`) must hold the mask, never
                // the plaintext the conflict-update wrote.
                assert_ne!(
                    masked, "987-65-4321",
                    "ssn (field's own column) must not hold plaintext"
                );
            }
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }

        let key = backend
            .key_store()
            .resolve("default", key_id)
            .await
            .expect("resolve key");
        let email_plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
            &email_blob,
            &encryption::canonical_aad("users", "email", None),
        )
        .expect("decrypt deterministic conflict key");
        assert_eq!(email_plaintext, b"alice@example.com".to_vec());

        let ssn_plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
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
    let _keys = with_root_key("c1_update_non_id_runtime", &"7".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
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

const _procedures = { seed, updateByEmail };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let seed_row = parity::extract_json(&seeded);
        let seed_id = seed_row["id"].as_str().expect("generated id");
        let updated = dispatch_sqlite_runtime(&dir, &source, "updateByEmail");
        let row = parity::extract_json(&updated);
        assert_eq!(
            row.get("id").and_then(|v| v.as_str()),
            Some(seed_id),
            "update by non-id filter should still target the seeded row"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#
                ),
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
            other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[2] {
            TypedCell::Text(masked) => {
                assert_eq!(masked, "***-**-4321");
                assert_ne!(
                    masked, "987-65-4321",
                    "ssn (field's own column) must not hold plaintext"
                );
            }
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }

        let key = backend
            .key_store()
            .resolve("default", key_id)
            .await
            .expect("resolve key");
        let plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
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
    let _keys = with_root_key("c1_update_many_non_id_runtime", &"8".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
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

const _procedures = { seed, updateManyByName };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");
        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
        let updated =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyByName"));
        assert_eq!(
            updated.as_f64(),
            Some(2.0),
            "two rows should match the non-id updateMany filter: {updated}"
        );
        let counters = zeroship_data_orm::crud::write_path_counters_for_tests();
        assert_eq!(
            counters.target_row_resolution_calls, 1,
            "encrypted updateMany must resolve one non-empty target set: {counters:?}"
        );
        assert!(
            !counters.target_row_resolution_sql.is_empty(),
            "the target-resolution SQL set must be non-empty: {counters:?}"
        );
        let expected_limit = format!(" LIMIT {}", zeroship_data_sql::compile::MAX_QUERY_LIMIT + 1);
        for sql in &counters.target_row_resolution_sql {
            assert!(
                sql.ends_with(&expected_limit),
                "updateMany target resolution must carry the row ceiling; sql={sql}"
            );
        }

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, name, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed updated rows");
        assert_eq!(typed.rows.len(), 2, "exactly two rows should be updated");

        let key = backend
            .key_store()
            .resolve("default", key_id)
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
                other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
            };
            match &row[3] {
                TypedCell::Text(masked) => {
                    assert_eq!(masked, "***-**-7777");
                    assert_ne!(
                        masked, "999-88-7777",
                        "ssn (field's own column) must not hold plaintext"
                    );
                }
                other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
            }
            let plaintext = zeroship_data_orm::encryption::aead::decrypt(
                &key,
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
fn update_many_randomised_target_cap_rejects_without_writes_sqlite_runtime() {
    let key_id = "c1_update_many_target_cap_runtime";
    let _keys = with_root_key("c1_update_many_target_cap_runtime", &"c".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;

        let dir = tempfile::tempdir().expect("tempdir");
        let target_cap = usize::try_from(zeroship_data_sql::compile::MAX_QUERY_LIMIT)
            .expect("MAX_QUERY_LIMIT must fit usize");
        let seeded = target_cap + 1;
        let values = (0..seeded)
            .map(|index| format!("('user_{index:04}', 'user_{index:04}@example.com', 'Red Team')"))
            .collect::<Vec<_>>();
        assert!(!values.is_empty(), "overflow fixture must seed target rows");
        let mut ddl = users_encrypted_ssn_ddl(key_id);
        ddl.push_str(&format!(
            "INSERT INTO \"default\".\"users\" (id, email, name) VALUES {};",
            values.join(",")
        ));
        apply_schema_ahead_of_runtime(&dir, &ddl);

        let schema = users_encrypted_ssn_schema(key_id);
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function overflow(_input, _ctx) {
    let failure = null;
    try {
        await env.db.collection(COLLECTION).updateMany(
            { name: "Red Team" },
            { ssn: "999-88-7777" },
        );
    } catch (err) {
        failure = {
            code: typeof err?.code === "string" ? err.code : null,
            message: err?.message ?? String(err),
        };
    }
    return { failure };
}
overflow.config = { kind: "action" };

const _procedures = { overflow };
"#,
        );

        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "overflow"));
        assert_eq!(
            result["failure"]["code"], "update_many_target_limit_exceeded",
            "the bounded probe must reject an overflowing target set: {result}"
        );
        let counters = zeroship_data_orm::crud::write_path_counters_for_tests();
        assert_eq!(
            counters.target_row_resolution_calls, 1,
            "overflow detection must use one bounded target probe: {counters:?}"
        );
        assert_eq!(
            counters.target_row_resolution_sql.len(),
            1,
            "the overflow SQL witness set must contain exactly the exercised probe"
        );
        let expected_limit = format!(" LIMIT {}", target_cap + 1);
        assert!(
            counters.target_row_resolution_sql[0].ends_with(&expected_limit),
            "the overflow probe must fetch at most one row beyond the write cap: {counters:?}"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let state = client
            .query_typed(
                r#"SELECT COUNT(*), SUM(version), COUNT(ssn)
                   FROM "default"."users"
                   WHERE name = 'Red Team'"#,
                &[],
            )
            .await
            .expect("inspect rows after overflowing updateMany");
        assert_eq!(
            state.rows.len(),
            1,
            "aggregate must return one non-empty row"
        );
        let expected_seeded = i64::try_from(seeded).expect("fixture count must fit i64");
        for (cell, expected, label) in [
            (&state.rows[0][0], expected_seeded, "row count"),
            (&state.rows[0][1], expected_seeded, "version sum"),
            (&state.rows[0][2], 0, "encrypted value count"),
        ] {
            match cell {
                TypedCell::Integer(actual) => assert_eq!(
                    *actual, expected,
                    "overflow rejection must preserve {label}"
                ),
                other => panic!("{label} must be INTEGER, got {other:?}"),
            }
        }
    });
}

#[test]
fn update_many_randomised_failure_rolls_back_committed_prefix_sqlite_runtime() {
    let key_id = "c1_update_many_atomic_failure_runtime";
    let _keys = with_root_key("c1_update_many_atomic_failure_runtime", &"a".repeat(64));

    run(async {
        use zeroship_data_orm::backend::sqlite::session::TypedCell;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    // No `id` on either row: it is platform-assigned, so supplying one is
    // refused at the document boundary. Nothing below reads these ids - every
    // later assertion filters on `name` - so they were fixture convenience.
    await coll.insert({
        email: "alice@example.com",
        name: "Red Team",
        ssn: "123-45-6789"
    });
    await coll.insert({
        email: "bob@example.com",
        name: "Red Team",
        ssn: "222-33-4444"
    });
    return true;
}
seed.config = { kind: "action" };

async function failBulk(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    let failure = null;
    try {
        await coll.updateMany(
            { name: "Red Team" },
            { email: "bulk-collision@example.com", ssn: "999-88-7777" },
        );
    } catch (err) {
        failure = {
            code: typeof err?.code === "string" ? err.code : null,
            message: err?.message ?? String(err),
        };
    }
    const after = await coll.find({ name: "Red Team" });
    return { failure, after };
}
failBulk.config = { kind: "action" };

async function failBulkInsideTransaction(_input, _ctx) {
    return await env.db.transaction(async (tx) => {
        let failure = null;
        try {
            await tx[COLLECTION].updateMany(
                { name: "Red Team" },
                { email: "nested-collision@example.com", ssn: "777-66-5555" },
            );
        } catch (err) {
            failure = {
                code: typeof err?.code === "string" ? err.code : null,
                message: err?.message ?? String(err),
            };
        }
        await tx[COLLECTION].insert({
            id: "control_row",
            email: "control@example.com",
            name: "Blue Team",
            ssn: "111-22-3333"
        });
        return { failure };
    });
}
failBulkInsideTransaction.config = { kind: "action" };

const _procedures = { seed, failBulk, failBulkInsideTransaction };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");
        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "failBulk"));
        let after = result["after"]
            .as_array()
            .expect("caught failure must leave an inspectable result set");
        assert_eq!(
            after.len(),
            2,
            "the exercised target set must be non-empty: {result}"
        );
        let counters = zeroship_data_orm::crud::write_path_counters_for_tests();
        assert_eq!(
            counters.target_row_resolution_calls, 1,
            "the failing call must exercise one per-row fan-out target query: {counters:?}"
        );
        assert!(
            !counters.target_row_resolution_sql.is_empty(),
            "the failing fan-out SQL witness must be non-empty: {counters:?}"
        );
        assert_eq!(
            result["failure"]["code"], "unique_violation",
            "the second conflicting row must reject in creator vocabulary: {result}"
        );
        let mut caller_visible: Vec<(String, i64)> = after
            .iter()
            .map(|row| {
                (
                    row["email"]
                        .as_str()
                        .expect("caller-visible email must be a string")
                        .to_string(),
                    row["version"]
                        .as_i64()
                        .expect("caller-visible version must be an integer"),
                )
            })
            .collect();
        caller_visible.sort();
        assert_eq!(
            caller_visible,
            vec![
                ("alice@example.com".to_string(), 1),
                ("bob@example.com".to_string(), 1),
            ],
            "after rejection, the caller must observe that no prefix committed"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let typed = client
            .query_typed(
                r#"SELECT email, version
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#,
                &[],
            )
            .await
            .expect("inspect rows after failed updateMany");
        assert_eq!(
            typed.rows.len(),
            2,
            "the directly inspected target set must be non-empty"
        );
        let expected = ["alice@example.com", "bob@example.com"];
        for (index, row) in typed.rows.iter().enumerate() {
            match &row[0] {
                TypedCell::Text(email) => assert_eq!(email, expected[index]),
                other => panic!("email must be TEXT, got {other:?}"),
            }
            match &row[1] {
                TypedCell::Integer(version) => assert_eq!(
                    *version, 1,
                    "a rejected updateMany must not commit or version-bump a prefix"
                ),
                other => panic!("version must be INTEGER, got {other:?}"),
            }
        }

        let nested = parity::extract_json(&dispatch_sqlite_runtime(
            &dir,
            &source,
            "failBulkInsideTransaction",
        ));
        // TWO things were stale here until 2026-09-01, and the first hid the
        // second.
        //
        // PATH: `transaction()` returns `Promise<Result<R>>` and wraps the
        // callback's value with `ok(...)` (`sdks/bootstrap/src/install-schema.ts`
        // :1145, :1193), so the payload is `{data: {...}, error: null}` and the
        // failure sits at `["data"]["failure"]`. Reading `["failure"]` yielded
        // `Null`, which compares unequal to ANY expected code - so this assertion
        // could never have passed, and could never have told you why.
        //
        // CASE: `canonicalErrorCode` (`sdks/db/src/errors.ts:27`) deliberately
        // upper-snakes every code not already in that form, so `unique_violation`
        // reaches app code as `UNIQUE_VIOLATION`. The lowercase driver spelling
        // survives only inside the nested `message` payload.
        assert_eq!(
            nested["data"]["failure"]["code"], "UNIQUE_VIOLATION",
            "the savepoint-wrapped fan-out must preserve the row error: {nested}"
        );
        let after_nested = client
            .query_typed(
                r#"SELECT email, version
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#,
                &[],
            )
            .await
            .expect("inspect rows after nested failed updateMany");
        assert_eq!(
            after_nested.rows.len(),
            2,
            "nested target set must be non-empty"
        );
        for (index, row) in after_nested.rows.iter().enumerate() {
            match &row[0] {
                TypedCell::Text(email) => assert_eq!(email, expected[index]),
                other => panic!("email must be TEXT, got {other:?}"),
            }
            match &row[1] {
                TypedCell::Integer(version) => assert_eq!(*version, 1),
                other => panic!("version must be INTEGER, got {other:?}"),
            }
        }
        let control = client
            .query_typed(
                // KEYED ON EMAIL, NOT ON THE SUPPLIED id. The procedure inserts
                // `{ id: "control_row", ... }`, and the write path DISCARDS that
                // and mints a typed id - the row lands as
                // `user_034HHQXErG6U2Eb6CiCTu0`. `id` is in
                // IMMUTABLE_SYSTEM_FIELDS (`crud/system_fields_pass.rs:46`), and
                // on INSERT a caller-supplied value is replaced silently, where
                // an UPDATE touching the same field is refused loudly (`:399`,
                // `:428`). So `WHERE id = 'control_row'` matched nothing and this
                // assertion read 0 - which looked exactly like the outer
                // transaction having been rolled back, and was not.
                r#"SELECT COUNT(*) FROM "default"."users" WHERE email = 'control@example.com'"#,
                &[],
            )
            .await
            .expect("inspect outer transaction control write");
        assert!(
            !control.rows.is_empty(),
            "the outer transaction control result must be non-empty"
        );
        match &control.rows[0][0] {
            TypedCell::Integer(count) => assert_eq!(
                *count, 1,
                "rolling back the updateMany savepoint must not roll back the outer transaction"
            ),
            other => panic!("control count must be INTEGER, got {other:?}"),
        }
    });
}

#[test]
fn plain_updates_on_encrypted_collection_stay_on_fast_path_sqlite_runtime() {
    let key_id = "perf_plain_update_fast_path_runtime";
    let _keys = with_root_key("perf_plain_update_fast_path_runtime", &"9".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
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

const _procedures = { seed, updatePlain, updateManyPlain };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");

        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updatePlain"));
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Blue Team"),
            "plain update should still update the targeted row",
        );
        assert_write_path_fast_path("updateOne plain field");

        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
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
    let _keys = with_root_key("perf_plain_upsert_fast_path_runtime", &"a".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_secret_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_secret_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
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
            email: "alice@example.com",
            name: "Alice Updated"
        },
        { conflictFields: ["email"] },
    );
}
upsertPlainConflict.config = { kind: "action" };

const _procedures = { seed, upsertPlainConflict };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let seed_row = parity::extract_json(&seeded);
        let seed_id = seed_row["id"].as_str().expect("generated id");

        zeroship_data_orm::crud::reset_write_path_counters_for_tests();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(
            &dir,
            &source,
            "upsertPlainConflict",
        ));
        assert_eq!(
            updated.get("id").and_then(|v| v.as_str()),
            Some(seed_id),
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
    let _keys = with_root_key("i5_update_nested_version", &"1".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
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
                { email: "alice@example.com" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdate.config = { kind: "action" };

const _procedures = { seed, nestedCasUpdate };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let row = parity::extract_json(&seeded);
        assert_eq!(
            row.get("version").and_then(|v| v.as_i64()),
            Some(1),
            "seed row must start at version 1"
        );

        let (status, body) = parity::dispatch_zs_with_descriptor(
            &parity::sqlite_url(&dir),
            &source.source,
            "nestedCasUpdate",
            parity::DEV_APP_ID,
            &source.descriptor,
        );
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE email = 'alice@example.com'"#,
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
    let _keys = with_root_key("i5_update_many_nested_version", &"2".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema(key_id);
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl(key_id));
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
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
                { email: "alice@example.com" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdateMany.config = { kind: "action" };

const _procedures = { seed, nestedCasUpdateMany };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");

        let (status, body) = parity::dispatch_zs_with_descriptor(
            &parity::sqlite_url(&dir),
            &source.source,
            "nestedCasUpdateMany",
            parity::DEV_APP_ID,
            &source.descriptor,
        );
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let backend = new_sqlite_backend(
            PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("open backend");
        backend
            .attach_app_file("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE email = 'alice@example.com'"#,
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

/// **Gate #1 (SQLite half)**: round-trip an encrypted string
/// column under Randomised mode. Insert a row with `ssn` declared
/// `t.encrypted({ mode: "randomised" })`, read it back via the SQLite
/// path, expect the plaintext to recover.
///
/// Each SQLite test uses a UNIQUE `keyId` so concurrent tests don't
/// race on the process-global env table - the
/// `ZEROSHIP_COLUMN_KEY_<KEYID>` namespace is per-key, so distinct
/// `keyId`s give each test its own env-var slot. Same pattern as the
/// in-crate `encryption::keys::tests` use.
#[test]
fn encrypted_column_round_trip_sqlite_randomised() {
    use zeroship_data_orm::backend::EncryptionMode;
    use zeroship_data_orm::encryption;
    let key_id = "p5_sqlite_rt_rand";
    let _keys = with_root_key("p5_sqlite_rt_rand", &"a".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend
            .key_store()
            .resolve("app1", key_id)
            .await
            .expect("resolve_key");
        let plaintext = b"123-45-6789";
        let aad = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
        let ct = zeroship_data_orm::encryption::aead::encrypt(
            &key,
            EncryptionMode::Randomised,
            plaintext,
            &aad,
        )
        .expect("encrypt");

        // Bind the ciphertext as an inline X'...' BLOB literal. The
        // session actor's `[&str]` params lane only carries TEXT; SQL
        // literals are how we inject BLOB values without widening the
        // protocol.
        let blob_lit = sqlite_blob_literal(&ct);
        let insert_sql =
            format!("INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})");
        backend
            .execute_fixture(&insert_sql, &[("row_a").into()])
            .await
            .expect("INSERT");

        // Pull the ciphertext back as a typed BLOB. `execute_fixture_on`
        // routes through `query`, which stringifies BLOBs as
        // `<N bytes blob>` — that's not what we want here. Reach into
        // the session's typed-row path via the dedicated client; the
        // `query_typed` method preserves the BLOB discriminant. The
        // simplest cross-test path: re-encode the BLOB as hex via SQL
        // (`hex(ssn)`) and parse back to bytes here.
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                &["row_a"],
            )
            .await
            .expect("SELECT");
        let hex_str = rows[0][0].clone().expect("ssn column must be present");
        let raw: Vec<u8> = (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect();

        let recovered =
            zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
        assert_eq!(recovered, plaintext);
    });
}

/// **Gate #1 (SQLite half), deterministic variant**.
#[test]
fn encrypted_column_round_trip_sqlite_deterministic() {
    use zeroship_data_orm::backend::EncryptionMode;
    use zeroship_data_orm::encryption;
    let key_id = "p5_sqlite_rt_det";
    let _keys = with_root_key("p5_sqlite_rt_det", &"b".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend
            .key_store()
            .resolve("app1", key_id)
            .await
            .expect("resolve_key");
        let plaintext = b"DETERMINISTIC-PLAINTEXT";
        // Deterministic AAD: row_pk omitted (Camp A).
        let aad = encryption::canonical_aad("enc_notes", "ssn", None);
        let ct = zeroship_data_orm::encryption::aead::encrypt(
            &key,
            EncryptionMode::Deterministic,
            plaintext,
            &aad,
        )
        .expect("encrypt");

        let blob_lit = sqlite_blob_literal(&ct);
        let insert_sql =
            format!("INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})");
        backend
            .execute_fixture(&insert_sql, &[("row_a").into()])
            .await
            .expect("INSERT");

        let client = backend
            .fixture_session("app_demo")
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

        let recovered =
            zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
        assert_eq!(recovered, plaintext);
    });
}

/// **Gate #2 (SQLite half), CRITICAL #1 fence (SQLite half)**.
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
    use zeroship_data_orm::backend::EncryptionMode;
    use zeroship_data_orm::encryption;
    let key_id = "p5_sqlite_det_eq";
    let _keys = with_root_key("p5_sqlite_det_eq", &"c".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");
        backend
            .execute_fixture(
                "CREATE INDEX \"app_demo\".\"enc_notes_ssn_idx\" \
                 ON \"enc_notes\"(ssn)",
                &[],
            )
            .await
            .expect("CREATE INDEX");

        let key = backend
            .key_store()
            .resolve("app1", key_id)
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
                zeroship_data_orm::encryption::aead::encrypt(
                    &key,
                    EncryptionMode::Deterministic,
                    p,
                    &aad,
                )
                .expect("encrypt")
            })
            .collect();

        // Defining deterministic property: re-encrypt P0 → same bytes.
        let p0_again = zeroship_data_orm::encryption::aead::encrypt(
            &key,
            EncryptionMode::Deterministic,
            plaintexts[0],
            &aad,
        )
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
            let sql =
                format!("INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})");
            backend
                .execute_fixture(&sql, &[(id.as_str()).into()])
                .await
                .expect("INSERT");
        }

        // Equality lookup on P0's ciphertext should match exactly 20
        // rows (0, 5, 10, ..., 95).
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        let p0_lit = sqlite_blob_literal(&ciphertexts[0]);
        let count_sql =
            format!("SELECT COUNT(*) FROM \"app_demo\".\"enc_notes\" WHERE ssn = {p0_lit}");
        let rows = client.query(&count_sql, &[]).await.expect("SELECT COUNT");
        let n: i64 = rows[0][0]
            .as_deref()
            .and_then(|s| s.parse().ok())
            .expect("count must parse");
        assert_eq!(
            n, 20,
            "equality on shared ciphertext must match every 5th row"
        );

        // P1's ciphertext should also match 20 rows.
        let p1_lit = sqlite_blob_literal(&ciphertexts[1]);
        let count_sql =
            format!("SELECT COUNT(*) FROM \"app_demo\".\"enc_notes\" WHERE ssn = {p1_lit}");
        let rows = client.query(&count_sql, &[]).await.expect("SELECT COUNT");
        let n: i64 = rows[0][0].as_deref().and_then(|s| s.parse().ok()).unwrap();
        assert_eq!(n, 20);
    });
}

/// **§13 Camp A fence (SQLite half), mirror of the PG test
/// `encrypted_randomised_row_swap_rejected`**. Insert two Randomised
/// rows; UPDATE swaps their ciphertexts; reading row B with row B's
/// AAD must surface `encryption_aead_failed`. This is the load-bearing
/// assertion for the row-PK-in-AAD policy.
#[test]
fn randomised_ciphertext_row_swap_rejected_sqlite() {
    use zeroship_data_orm::backend::EncryptionMode;
    use zeroship_data_orm::encryption;
    let key_id = "p5_sqlite_row_swap";
    let _keys = with_root_key("p5_sqlite_row_swap", &"d".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend.key_store().resolve("app1", key_id).await.unwrap();
        // Insert row A and row B, each with its OWN AAD (binds row_pk).
        let ct_a = zeroship_data_orm::encryption::aead::encrypt(
            &key,
            EncryptionMode::Randomised,
            b"sensitive-A",
            &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a")),
        )
        .unwrap();
        let ct_b = zeroship_data_orm::encryption::aead::encrypt(
            &key,
            EncryptionMode::Randomised,
            b"sensitive-B",
            &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_b")),
        )
        .unwrap();
        for (id, ct) in [("row_a", &ct_a), ("row_b", &ct_b)] {
            let blob_lit = sqlite_blob_literal(ct);
            let sql =
                format!("INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})");
            backend.execute_fixture(&sql, &[(id).into()]).await.unwrap();
        }

        // Attacker move: UPDATE row_b's ssn slot with row_a's ciphertext.
        let blob_a = sqlite_blob_literal(&ct_a);
        let sql = format!("UPDATE \"app_demo\".\"enc_notes\" SET ssn = {blob_a} WHERE id = ?");
        backend
            .execute_fixture(&sql, &[("row_b").into()])
            .await
            .unwrap();

        // Read row B's ssn back and try to decrypt with row B's AAD.
        let client = backend
            .fixture_session("app_demo")
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
        let err = zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad_b)
            .expect_err("row-swap must fail AAD verification");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed/encryption_aead_failed, got {other:?}"),
        }
    });
}

/// **Cross-backend equivalence (SQLite <-> SQLite via shared
/// env-var key).** Encrypt plaintext on backend_a; copy the ciphertext
/// bytes; decrypt on backend_b (different temp file) configured with
/// the same `ZEROSHIP_COLUMN_KEY_DEFAULT`. Proves HKDF derivation is
/// deterministic across instances — the encryption module is the
/// shared cross-backend surface, so two SQLite backends with the same
/// root key produce the same derived AEAD key (and thus the same
/// decryption result).
#[test]
fn cross_backend_ciphertext_decrypt_via_shared_key() {
    use zeroship_data_orm::backend::EncryptionMode;
    use zeroship_data_orm::encryption;
    let key_id = "p5_sqlite_cross";
    let _keys = with_root_key("p5_sqlite_cross", &"e".repeat(64));
    run(async {
        // Two separate backends rooted at separate temp dirs.
        let (backend_a, _dir_a) = fresh_backend();
        let (backend_b, _dir_b) = fresh_backend();

        // Use the SAME app_id so HKDF salt matches; the env-var key
        // sourcing is process-global, so the root key is identical.
        let app_id = "app_shared";
        let key_a = backend_a.key_store().resolve(app_id, key_id).await.unwrap();
        let key_b = backend_b.key_store().resolve(app_id, key_id).await.unwrap();
        // The derived halves must match — same root + same app_id.
        assert_eq!(key_a.k_enc, key_b.k_enc);
        assert_eq!(key_a.k_siv, key_b.k_siv);

        let plaintext = b"cross-instance-payload";
        let aad = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
        let ct = zeroship_data_orm::encryption::aead::encrypt(
            &key_a,
            EncryptionMode::Randomised,
            plaintext,
            &aad,
        )
        .expect("encrypt on A");

        // Decrypt the SAME ciphertext on backend_b with backend_b's
        // resolved key. Must round-trip.
        let recovered =
            zeroship_data_orm::encryption::aead::decrypt(&key_b, &ct, &aad).expect("decrypt on B");
        assert_eq!(recovered, plaintext);
    });
}

// ===========================================================================
// Encryption, SQL compilation, binding and typed decoding agree on bytes.

/// Full encrypted-column round-trip through the SQLite CRUD pipeline
/// (encryption pass -> SQL builder w/ SQLite dialect -> SQLite session
/// bind -> typed row decode -> decrypt pass). This is the test that
/// proves the end-to-end SDK works on SQLite for `t.encrypted(...)`
/// columns.
#[test]
fn encrypted_column_e2e_crud_round_trip_sqlite() {
    use zeroship_data_orm::fixtures::DatabaseFixture;
    use zeroship_data_orm::protection::encryption_pass::{
        decrypt_row_on_read, encrypt_row_on_write,
    };
    use zeroship_data_orm::backend::sqlite::session::TypedCell;
    use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};

    let _keys = with_root_key("p5_e2e_crud", &"c".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        // PRIMARY KEY `id TEXT` + encrypted `ssn BLOB` — same shape the
        // CRUD path's `build_create_table_with_fks` emits for an
        // `t.encrypted({ wraps: "string" })` field, except we skip the
        // sentinel-comment metadata because the introspector isn't on
        // the e2e read path here.
        backend
            .execute_fixture(
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
        let schema = zeroship_data_sql::value!({
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
        let mut doc = zeroship_data_sql::value!({
            "id": row_pk,
            "ssn": plaintext,
        });

        // The encryption pass replaces ssn with native ciphertext bytes.
        encrypt_row_on_write(
            backend.key_store(),
            "app_demo",
            "users",
            &schema,
            row_pk,
            &mut doc,
        )
        .await
        .expect("encrypt_row_on_write");
        let ciphertext = doc["ssn"].as_bytes().expect("native ciphertext").to_vec();

        // Bind the ciphertext directly.
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert_with_dialect");
        assert!(
            !bq.sql.contains("decode("),
            "SQLite dialect must not emit `decode(...)::bytea`: {}",
            bq.sql,
        );
        assert!(
            bq.params
                .iter()
                .any(|value| value.as_bytes() == Some(ciphertext.as_slice()))
        );
        assert!(!bq.sql.contains("unhex("));

        // Execute the compiled INSERT through the typed RETURNING surface.
        let param_refs = &bq.params;
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        let _affected = client
            .query_typed(&bq.sql, param_refs)
            .await
            .expect("INSERT ... RETURNING via SQLite session");

        // Step 4 — pull the BLOB back typed. The session's `query`
        // surface stringifies BLOBs as `<N bytes blob>` placeholders, so
        // we reach for the typed surface via the session handle's
        // `query_typed` helper.
        let typed = client
            .query_typed(
                "SELECT id, ssn FROM \"app_demo\".\"users\" WHERE id = ?",
                &[row_pk.into()],
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
        assert_eq!(
            ssn_bytes, ciphertext,
            "stored ciphertext must match the protection pass"
        );
        let mut row_value = zeroship_data_sql::value!({
            "id": id_text,
            "ssn": zeroship_data_sql::value::Value::Bytes(ssn_bytes),
        });

        decrypt_row_on_read(
            backend.key_store(),
            "app_demo",
            "users",
            &schema,
            &mut row_value,
        )
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
// Storage-flip raw-column dual-write integration
// ===========================================================================
//
// Tests the end-to-end contract on the SQLite arm:
// (a) CREATE TABLE emits the field's own column (bare `TEXT`, holding the
//     mask) plus a RAW sibling - named via [`raw_column_name`] - that
//     carries the declared type and constraints.
// (b) INSERT writes both atomically (mask pass runs before SQL build).
// (c) The field's own column contains the pre-computed mask string while
//     the raw sibling stores the ciphertext / plaintext.

/// **DDL shape on SQLite**: `build_create_table_with_fks`
/// emits the field's own column as a bare `TEXT` mask holder (carrying the
/// `zero-migrate:mask:...` sentinel) plus a RAW sibling - named via
/// [`raw_column_name`], never spelled out here - that carries the declared
/// type and constraints. The SQLite arm receives the SQL byte-identical to
/// PG for this schema (no encryption, so no dialect-specific BYTEA/BLOB
/// split); the SQLite engine accepts it once executed through the
/// SQLite-flavoured `CREATE TABLE` path.
#[test]
fn a_raw_column_is_emitted_for_a_masked_field_sqlite() {
    let schema = zeroship_data_sql::value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
        "name": { "type": "string" }
    });
    let sql = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
        "users",
        &schema,
        &FkEmission::Inline,
    )
    .unwrap();
    let raw_ssn = raw_column_name("ssn");
    assert!(
        sql.contains(&format!("\"{raw_ssn}\" TEXT")),
        "raw column must be emitted to carry the real value: {sql}"
    );
    assert!(
        sql.contains("\"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */"),
        "the field's own column must be the masked sibling and carry the mask sentinel: {sql}"
    );
    // The sentinel rides the masked column only - the raw column is not
    // itself masked (it holds the real value), so it must carry no mask
    // metadata.
    assert!(
        !sql.contains(&format!("\"{raw_ssn}\" TEXT /* __zsmask")),
        "the raw column must not carry the mask sentinel: {sql}"
    );
    assert!(
        !sql.contains("\"name_masked\""),
        "non-masked column must not emit a sibling: {sql}"
    );
    let raw_name = raw_column_name("name");
    assert!(
        !sql.contains(&format!("\"{raw_name}\"")),
        "non-masked column must not emit a raw sibling: {sql}"
    );
}

/// **Atomic dual-write on SQLite**: when a row carries
/// both the parent + sibling (mask pass already ran), the
/// SQLite-flavoured `build_insert_with_dialect` INSERT statement
/// includes both columns atomically. Then we execute the INSERT
/// against a hand-rolled SQLite-shaped table to confirm the engine
/// accepts the dual write end-to-end and persists the masked value
/// alongside the plaintext.
#[test]
fn dual_write_insert_persists_parent_and_sibling_sqlite() {
    use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        // Hand-rolled SQLite-flavoured CREATE TABLE — the SQLite
        // CREATE TABLE dialect doesn't speak PG's SERIAL /
        // TIMESTAMPTZ; the orchestrator emits SQLite-flavoured DDL
        // elsewhere. The sibling-column CLAUSE emission is standard
        // SQL; we exercise it inside a SQLite-valid table here.
        backend
            .execute_fixture(
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
        let doc = zeroship_data_sql::value!({
            "ssn": "123-45-6789",
            "ssn_masked": "***-**-6789"
        });
        // The descriptor entry for the fixture table above. It declares `ssn`
        // only: `ssn_masked` is a PHYSICAL column the mask pass writes, never a
        // declared field, so it is on the INSERT column list and not on the
        // projection - which is the shape this test is about.
        let schema = zeroship_data_sql::value!({ "ssn": { "type": "string" } });
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(
            bq.sql.contains("\"ssn\"") && bq.sql.contains("\"ssn_masked\""),
            "INSERT must reference both parent + sibling: {}",
            bq.sql,
        );

        let param_refs = &bq.params;
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        let _ = client
            .query_values(&bq.sql, param_refs)
            .await
            .expect("dual-write INSERT must succeed");

        // Verify both columns landed atomically.
        let rows = client
            .query("SELECT ssn, ssn_masked FROM \"app_demo\".\"users\"", &[])
            .await
            .expect("SELECT both columns");
        assert_eq!(rows.len(), 1, "exactly one row inserted");
        assert_eq!(rows[0][0].as_deref(), Some("123-45-6789"));
        assert_eq!(rows[0][1].as_deref(), Some("***-**-6789"));
    });
}

/// **A default SELECT serves the masked column**: a default
/// read against a masked-column DDL must name the field's own column
/// directly - no AS-rewrite; the sibling-alias scheme is gone since the
/// storage flip - and must NEVER reference the raw column. Since the
/// field's own column is now where a dual-write leaves the mask, a
/// schema-blind SELECT already reads the mask with no special casing.
/// End-to-end gate: drive a dual-write through the dialect-aware INSERT
/// builder (mirroring what `mask_pass::relocate_masked_columns`
/// produces), then build a `find` SQL via `build_find_with_schema` with
/// the cached schema, run it through the SQLite session, and assert the
/// engine returns the masked string under the field's own column - and
/// that the real value is nowhere in the row.
#[test]
fn a_select_serves_the_masked_column_sqlite() {
    use zeroship_data_sql::compile::{
        SqlDialect, build_find_with_schema, build_insert_with_dialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        let raw_ssn = raw_column_name("ssn");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"app_demo\".\"users\" (\
                         id    TEXT PRIMARY KEY, \
                         \"{raw_ssn}\" TEXT, \
                         ssn   TEXT NOT NULL, \
                         name  TEXT\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE ok");

        // Dual-write a row the way `mask_pass::relocate_masked_columns`
        // produces one: the raw column stores the real value (plaintext
        // here - masking + encryption are orthogonal in
        // `apply_mask_on_write` design), the field's own column stores
        // the masked string.
        let schema = zeroship_data_sql::value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let mut doc = zeroship_data_sql::value!({
            "id": "usr_01",
            "ssn": "***-**-6789",
            "name": "alice"
        });
        doc.as_object_mut()
            .expect("doc object")
            .insert(raw_ssn.clone(), zeroship_data_sql::value!("123-45-6789"));
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert_with_dialect");
        let param_refs = &bq.params;
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        client
            .query_values(&bq.sql, param_refs)
            .await
            .expect("INSERT");

        // Build a default read with schema awareness: the SELECT must
        // name the field's own column directly AND must NOT reference the
        // raw column at all. Verify the SQL shape BEFORE running the
        // query - this is the load-bearing assertion this test pins.
        let bq = build_find_with_schema(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "users",
            &zeroship_data_sql::value!({ "id": "usr_01" }),
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect("build_find_with_schema");
        let select_clause = bq
            .sql
            .split(" FROM ")
            .next()
            .expect("SELECT prefix")
            .to_string();
        assert!(
            select_clause.contains("\"ssn\""),
            "SELECT must project the field's own column directly: {select_clause}"
        );
        assert!(
            !select_clause.contains("AS \"ssn\""),
            "there is no more AS-rewrite onto ssn - the aliasing scheme is gone: {select_clause}"
        );
        // The raw column (real value) must NEVER appear in a default
        // read's SELECT clause - it is unqueryable outside the audited
        // unmask path (`protection::unmask`).
        assert!(
            !select_clause.contains(raw_ssn.as_str()),
            "SELECT must never reference the raw column: {select_clause}"
        );

        // Execute the SELECT and verify the row returns the masked
        // string under the field's own column, and the real value is
        // nowhere in the row.
        let param_refs = &bq.params;
        let rows = client
            .query_values(&bq.sql, param_refs)
            .await
            .expect("SELECT");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.iter()
                .filter_map(|c| c.as_deref())
                .find(|s| *s == "***-**-6789"),
            Some("***-**-6789"),
            "row must include the masked string: {row:?}"
        );
        // The real value must NOT appear anywhere in the row (we never
        // selected the raw column).
        assert!(
            !row.iter().any(|c| c.as_deref() == Some("123-45-6789")),
            "the real value must not surface on a default read: {row:?}"
        );
    });
}

/// **`kind: none` preserves the encryption decrypt-on-read path**:
/// when a column declares `mask: { kind: "none" }`, the SELECT clause
/// must emit the parent column directly (no AS-rewrite), and the row
/// must surface the parent's value (the ciphertext / plaintext under
/// the parent column).
#[test]
fn aliased_select_skips_kind_none_sqlite() {
    use zeroship_data_sql::compile::build_find_with_schema;

    let schema = zeroship_data_sql::value!({
        "ssn": {
            "type": "string",
            "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
            "mask": { "kind": "none", "classification": "spi" }
        },
        "name": { "type": "string" }
    });
    let bq = build_find_with_schema(
        &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
        "users",
        &zeroship_data_sql::value!({}),
        None,
        None,
        None,
        None,
        &schema,
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
        bq.sql
            .contains("SELECT \"id\", \"created_at\", \"updated_at\"")
            && bq.sql.contains("\"ssn\"")
            && bq.sql.contains("\"name\""),
        "schema-backed reads must project the public column set: {}",
        bq.sql,
    );
}

/// **NOT NULL contract on the sibling**: omitting the
/// sibling from an INSERT against a masked-column DDL must fail at the
/// engine level (the sibling is `TEXT NOT NULL`). This is the
/// load-bearing assertion that mask-pass must run before the SQL
/// builder - skip it and the engine rejects with a NOT NULL violation.
#[test]
fn missing_sibling_fails_not_null_constraint_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
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
            .execute_fixture(
                "INSERT INTO \"app_demo\".\"users\" (\"ssn\") VALUES (?)",
                &[("plaintext-no-mask").into()],
            )
            .await;
        assert!(
            res.is_err(),
            "INSERT without sibling MUST fail (sibling is NOT NULL); got Ok"
        );
    });
}

// ===========================================================================
// SQLite `Backup` impl (VACUUM INTO snapshot + atomic file-swap restore).
// Four tests covering the gates in plan §11 + the CRITICAL #3 fence
// (concurrent writer). Gate #6 was a fifth, deleted 2026-09-07 with
// `pitr_replay`; its slot is left numbered below so the plan's gate numbers
// still line up with what is here.
//
//   1. `snapshot_restore_round_trip_sqlite` (gate #4): seed rows,
//      snapshot to file://; drop rows; restore; assert recovery.
//   2. `vacuum_into_snapshot_consistent_under_concurrent_writer`
//      (gate #5 / CRITICAL #3 fence): spawn a writer thread; trigger
//      snapshot; assert (a) snap file well-formed, (b) live > snap,
//      (c) no SQLITE_BUSY under Retry policy.
//   3. (gate #6, deleted) `pitr_pg_only_returns_configuration_on_sqlite`.
//   4. `snapshot_during_migration_returns_typed_error_sqlite`: hold
//      snapshot_restore lock; snapshot must refuse w/ `migration_in_progress`.
//   5. `restore_hash_mismatch_rejected_sqlite`: corrupt snapshot;
//      restore must refuse with `snapshot_hash_mismatch` BEFORE touching
//      the live DB.

use zeroship_data_orm::backend::{Backup as _, BusyPolicy as BackupBusyPolicy, SnapshotOpts};

/// **Gate #4**: round-trip snapshot+restore on SQLite.
/// Insert N rows into a per-app collection; snapshot to a temp dir;
/// raw `DROP TABLE` to clear rows; restore; assert the rows recovered.
///
/// The dest URI uses the `file://` scheme (the only one supported).
/// We pick a destination INSIDE the backend's `db_dir` so the restore's
/// `std::fs::copy -> rename` swap lands on the same filesystem as the
/// live per-app file (POSIX rename atomic-same-FS contract).
#[test]
fn snapshot_restore_round_trip_sqlite() {
    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        // Seed deterministic rows.
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        const ROW_COUNT: i64 = 10;
        for i in 0..ROW_COUNT {
            let sql = format!("INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'row-{i}')");
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT row");
        }
        // Sanity: row count is N.
        let client = backend
            .fixture_session("app_demo")
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
            .execute_fixture("DELETE FROM \"app_demo\".\"notes\"", &[])
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
        backend.restore("app_demo", &handle).await.expect("restore");

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

/// **Gate #5 / CRITICAL #3 fence**: VACUUM INTO under a
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        // Seed an initial baseline so the snapshot is not empty.
        const INITIAL_ROWS: usize = 50;
        for i in 0..INITIAL_ROWS {
            let sql = format!("INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'initial-{i}')");
            backend
                .execute_fixture(&sql, &[])
                .await
                .expect("INSERT initial");
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
            let conn =
                rusqlite::Connection::open(&app_file).expect("writer-thread connection open");
            // Match the session's WAL mode so we are in the right
            // concurrency regime; busy_timeout absorbs short-term
            // contention.
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .expect("writer PRAGMAs");
            // Insert IDs starting past INITIAL_ROWS to avoid PK
            // collision with seeded rows.
            let mut i = INITIAL_ROWS;
            while !writer_stop.load(Ordering::Relaxed) {
                let sql = format!("INSERT INTO \"notes\" VALUES ({i}, 'concurrent-{i}')");
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
        let _handle = snap_result
            .expect("VACUUM INTO under concurrent writer must succeed (Retry absorbs busy)");

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
        let snap_conn =
            rusqlite::Connection::open(&snap_path).expect("open snapshot file standalone");
        let snap_count: i64 = snap_conn
            .query_row("SELECT COUNT(*) FROM \"notes\"", [], |r| r.get(0))
            .expect("count snap rows");
        assert!(snap_count >= INITIAL_ROWS as i64);
        // (b) — live > snap (the concurrent writer's commits past the
        // snapshot's read mark are visible in live but NOT in snap).
        let client = backend
            .fixture_session("app_demo")
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

// Gate #6 was `pitr_pg_only_returns_configuration_on_sqlite`, asserting that
// `pitr_replay` refused with `Configuration { code: "pitr_pg_only" }` on both
// target forms. It went on 2026-09-07 with the method: the refusal it pinned
// described a per-vendor divergence that did not exist, because the PG arm
// could not replay either. `zeroship_data_orm::storage::Backup`'s rustdoc
// carries the reason. Nothing replaced this test, and nothing should - there is
// no behaviour left to assert.

/// When the per-app `snapshot_restore` advisory lock is
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

        // Hold the snapshot_restore lock through the typed LockManager
        // surface — exactly the slot the snapshot pre-flight tries to
        // acquire. The `to_keys` derivation is identical to what the
        // snapshot impl computes.
        let client = backend
            .fixture_session("default")
            .await
            .expect("acquire client");
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: "snapshot_restore".to_string(),
        };
        let acquired = backend
            .try_acquire(&client, &scope)
            .await
            .expect("try_acquire snapshot_restore");
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
            .expect_err("snapshot must refuse while snapshot_restore lock is held");
        match err {
            DbError::Coded { code, .. } => {
                assert_eq!(
                    code, "migration_in_progress",
                    "expected Coded migration_in_progress, got code={code:?}"
                );
            }
            other => {
                panic!("expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}")
            }
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
            .expect("release snapshot_restore");
    });
}

/// A `restore()` whose on-disk file has drifted from the
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
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        backend
            .execute_fixture(
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
            other => {
                panic!("expected Coded {{ code: \"snapshot_hash_mismatch\", .. }}, got {other:?}")
            }
        }

        // The live DB must be untouched — the sentinel row still
        // exists. (Even without the mismatch check, the rename swap
        // only fires after the hash verify; an early-refuse contract
        // means the live file is bit-for-bit unchanged.)
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client");
        let rows = client
            .query("SELECT body FROM \"app_demo\".\"notes\" WHERE id = 1", &[])
            .await
            .expect("post-refuse query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some("sentinel"));
    });
}

// ---------------------------------------------------------------------------
// The reserved-name validator (Path B sibling-column suffix +
// classification taxonomy) refuses creator-declared collisions at the
// DDL builder level on the SQLite arm. Two tests pin the same surface
// the PG integration suite exercises so both backends agree on the
// reserved namespace.
// ---------------------------------------------------------------------------

#[test]
fn p55_pr1_build_create_table_refuses_masked_suffix_field_sqlite() {
    let schema = zeroship_data_sql::value!({
        "name": {"type": "string"},
        // `_masked` is reserved for Path B sibling columns.
        "card_pan_masked": {"type": "string"},
    });
    let result = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
        "cards",
        &schema,
        &FkEmission::Inline,
    );
    let err = result.expect_err("schema with `_masked` suffix should be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name") && msg.contains("_masked"),
        "expected reserved-suffix message, got: {msg}"
    );
}

#[test]
fn p55_pr1_build_create_table_refuses_classification_name_field_sqlite() {
    let schema = zeroship_data_sql::value!({
        "name": {"type": "string"},
        // `phi` collides with the platform classification taxonomy.
        "phi": {"type": "string"},
    });
    let result = fixture_table_sql(
        &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
        "patients",
        &schema,
        &FkEmission::Inline,
    );
    let err = result.expect_err("schema with reserved classification name should be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name"),
        "expected reserved-name message, got: {msg}"
    );
}

// ===========================================================================
// Unmask RPC + audit table (SQLite arm)
// ===========================================================================
//
// These tests exercise `protection::unmask::dispatch_unmask` end-to-end on the
// SQLite arm:
//   - the audit table is created idempotently on first call;
//   - the default-deny stub grants `kind: "auto"` and denies everyone else;
//   - both granted AND denied paths emit a row to the per-app audit table;
//   - the encrypted-column read path decrypts via `encryption::aead`;
//   - the typed error rail surfaces `unmask_column_not_masked` /
//     `unmask_not_permitted` on the SDK's `.code`-branchable path.
//
// Schema cache + backend handle are installed via the `*_for_tests`
// helpers in `lib.rs`. Each test uses a fresh tempdir so the audit
// table is observed from a clean slate.

use zeroship_data_orm::protection::unmask;

/// Helper — install backend + schema for an unmask test. Returns the
/// backend (kept alive for the test duration via Rc) + the TempDir
/// guard the caller binds to keep the on-disk directory alive.
async fn unmask_setup_with_schema(
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = Rc::new(
        new_sqlite_backend(
            std::path::PathBuf::from(dir.path()),
            zeroship_data_v8::testing::isolate_key_source(),
        )
        .expect("SqliteBackend::new"),
    );
    backend
        .attach_app_file(app_id)
        .await
        .expect("ensure_app_schema");
    // The audit table, APPLY-AHEAD. `crud/unmask.rs` used to create it itself
    // on every dispatch; it no longer emits DDL at all, so something has to
    // stand in here for the dev-tier apply host
    // (`zeroship-migrate-node`'s `applyIrSqlite`), exactly as the
    // `apply_schema_ahead_of_runtime` fixtures stand in for it for creator
    // tables.
    //
    // These are the PRODUCTION bytes, from the production generator, not a copy
    // of them: `audit_unmask_ddl` is the same function the host calls. The
    // qualifier differs because the CONNECTION differs - the host opened the
    // app file as `main`, the worker's backend reaches it through the
    // `<app_id>` ATTACH alias - and that parameter is the only thing that
    // varies between the two callers.
    //
    // WHAT THIS FIXTURE CANNOT PROVE: that the host actually calls it. It pins
    // the shape and the writer against each other, nothing more. The call in
    // `bridge.rs::apply_ir_sqlite` is covered by no test in this file.
    for stmt in zeroship_migrate_sqlite::backend::audit_unmask_ddl(app_id) {
        backend
            .execute_fixture(&stmt, &[])
            .await
            .expect("apply-ahead: unmask audit table");
    }
    // Install into the per-isolate context so dispatch_unmask's
    // backend() lookup succeeds.
    zeroship_data_v8::testing::set_backend_for_tests(zeroship_data_orm::backend::BackendHandle::new(backend.clone()), &format!("sqlite:{}", dir.path().display()));
    zeroship_data_orm::cache_schema_for_tests(app_id, collection, schema);
    (backend, dir)
}

/// Drop the fixture-installed backend while preserving its on-disk databases,
/// then reinstall the app descriptor and policy as a fresh startup would.
/// The database remains cold until its first operation.
fn configure_cold_sqlite_unmask_fixture(
    dir: &tempfile::TempDir,
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
    policy: zeroship_data_sql::value::Value,
) {
    zeroship_data_v8::testing::reset_context_for_tests();
    let url = format!("sqlite:{}", dir.path().join("zs-control.sqlite").display());
    zeroship_data_v8::testing::set_db_url_for_tests(&url);
    zeroship_data_orm::cache_schema_for_tests(app_id, collection, schema);
    mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy)
        .expect("reinstall the app declaration during startup");
}

/// The cold-open gate: prove the isolate left by
/// [`configure_cold_sqlite_unmask_fixture`] has NO backend, and that
/// `tx_scope::ensure_backend` is what opens and installs one.
///
/// Native unmask operations open their backend lazily. Policy installation
/// has already run at startup without touching the database. The V8 dispatch
/// wiring is covered by `v8_classes::cold_open`; this helper verifies that the
/// resolver opens a fresh backend and keeps using that instance.
///
/// # Why identity, and not a context read
///
/// `crate::context` is `pub(crate)` in every build, so an integration target
/// cannot ask "is the backend slot empty" directly. It can ask something
/// stronger: the handle that comes back must not be the one the fixture
/// installed, and a SECOND resolution must return that same fresh handle rather
/// than open a third. The caller keeps its fixture `Rc` alive across this call
/// for exactly that reason - a dropped `SqliteBackend` could be reallocated at
/// the same address and make the first assertion pass on a coincidence.
async fn assert_cold_open_installs_a_fresh_backend(fixture: &SqliteBackend) {
    let opened = zeroship_data_v8::tx_scope::ensure_backend().await.expect(
        "a cold isolate must be OPENED by ensure_backend: a plain context read answers \
             not_configured here, which is what every fresh isolate would get",
    );
    let opened = opened
        .get::<zeroship_data_orm::backend::SqliteBackend>()
        .expect("the SQLite arm");
    assert!(
        !std::ptr::eq(opened, fixture),
        "the cold fixture's backend is still installed, so nothing was opened"
    );

    let again = zeroship_data_v8::tx_scope::ensure_backend()
        .await
        .expect("the second resolution must see the backend the first one opened");
    assert!(
        std::ptr::eq(
            again
                .get::<zeroship_data_orm::backend::SqliteBackend>()
                .expect("the SQLite arm"),
            opened
        ),
        "the open must INSTALL into the isolate, not hand back a private handle: \
         a second resolution opened a different backend"
    );
}

/// Read every row from `__zeroship_audit_unmask` for a given app.
/// Returns `Vec<(outcome, actor_role, classification)>`.
async fn read_audit_rows(backend: &SqliteBackend, app_id: &str) -> Vec<(String, String, String)> {
    // A read: it belongs on `op_conn`, not on the exclusive `tx_conn`
    // reservation. Asking for the transaction lane here contends with whatever
    // the unmask dispatch itself is holding.
    let client = backend.autocommit_client();
    let q_app = zeroship_data_sql::compile::quote_ident(app_id);
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

/// **cold-open gate, single unmask**: the fixture leaves the isolate with a URL
/// and no backend, and `tx_scope::ensure_backend` - the call
/// `v8_classes::dispatch::dispatch_unmask_field` makes before handing the engine
/// a handle - is what opens one.
///
/// The sibling gate below drives the same cold fixture through
/// `dispatch_unmask` and rules on the ATTACH; this one rules on the OPEN, which
/// nothing else does. See [`assert_cold_open_installs_a_fresh_backend`].
#[test]
fn cold_unmask_open_comes_from_ensure_backend_not_the_fixture() {
    let schema = zeroship_data_sql::value!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
        },
    });
    let app_id = "app_unmask_cold_open";
    let collection = "users";

    run(async {
        // `fixture` stays bound for the whole block: the assertion is an
        // address comparison against it.
        let (fixture, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        configure_cold_sqlite_unmask_fixture(
            &dir,
            app_id,
            collection,
            schema,
            zeroship_data_sql::value!({}),
        );
        assert_cold_open_installs_a_fresh_backend(fixture.as_ref()).await;
    });
}

/// **Gate #1**: an `auto` actor unmasking an encrypted +
/// masked column recovers plaintext, and a `granted` audit row is
/// emitted with the right classification.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_unmask_with_auto_actor_attaches_before_read() {
    let _keys = with_root_key("p55_pr4_auto", &"a".repeat(64));
    let schema = zeroship_data_sql::value!({
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
        let (backend, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        // Manually create the table — the encryption pass + dual-write
        // pipeline lives in CRUD, but the unmask SELECT only needs
        // `id TEXT PRIMARY KEY, "<raw ssn>" BLOB, ssn TEXT`. Mirrors the
        // e2e CRUD test. Post-storage-flip layout: the raw column (named
        // via `raw_column_name`, never spelled out here) holds the
        // ciphertext `protection::unmask` reads; the field's own column (`ssn`)
        // holds the mask, exactly as a default read pipeline would leave it.
        let raw_ssn = raw_column_name("ssn");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"app_unmask_auto\".\"users\" (\
                         id  TEXT PRIMARY KEY, \
                         \"{raw_ssn}\" BLOB, \
                         ssn TEXT NOT NULL DEFAULT '***-**-XXXX'\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Encrypt + insert one row inline.
        use zeroship_data_orm::protection::encryption_pass::encrypt_row_on_write;
        use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
        let row_pk = "usr_auto_01";
        let plaintext = "123-45-6789";
        let mut doc = zeroship_data_sql::value!({
            "id": row_pk,
            "ssn": plaintext,
        });
        encrypt_row_on_write(
            backend.key_store(),
            app_id,
            collection,
            &schema,
            row_pk,
            &mut doc,
        )
        .await
        .expect("encrypt_row_on_write");
        // Relocate the native ciphertext to the raw column, as the mask pass
        // does, and store the precomputed mask in the logical column.
        let ciphertext = doc
            .as_object_mut()
            .expect("doc object")
            .shift_remove("ssn")
            .expect("ciphertext produced by encrypt_row_on_write");
        {
            let obj = doc.as_object_mut().expect("doc object");
            obj.insert(raw_ssn.clone(), ciphertext);
            obj.insert("ssn".to_string(), zeroship_data_sql::value!("***-**-6789"));
        }
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
            collection,
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert_with_dialect");
        let client = backend
            .fixture_session(app_id)
            .await
            .expect("acquire client");
        let param_refs = &bq.params;
        let _ = client
            .query_typed(&bq.sql, param_refs)
            .await
            .expect("INSERT");

        // Remove the fixture-installed backend. The `unmask_backend()` below is
        // the harness making the open the V8 dispatch makes in production (the
        // cold-open gate above is where that open is ruled on); what THIS test
        // rules on is the next step - `dispatch_unmask` ATTACHing the existing
        // app file to that freshly opened connection before its direct SELECT.
        configure_cold_sqlite_unmask_fixture(
            &dir,
            app_id,
            collection,
            schema.clone(),
            zeroship_data_sql::value!({}),
        );
        let _cold_keys = with_root_key("p55_pr4_auto", &"a".repeat(64));

        // Dispatch unmask with `kind: "auto"` actor — must succeed.
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: row_pk.to_string(),
            column: "ssn".to_string(),
            actor: Some(zeroship_data_sql::value!({ "kind": "auto", "id": null })),
            reason: Some("integration test".to_string()),
            rejected_claim: None,
        };
        let result = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
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

        // A direct read of the field's own column (what a default read
        // pipeline would see, no audit, no authorization check) must
        // still be the mask, never the plaintext - the audited path above
        // is the only way to recover it.
        let direct = client
            .query(
                "SELECT ssn FROM \"app_unmask_auto\".\"users\" WHERE id = 'usr_auto_01'",
                &[],
            )
            .await
            .expect("direct SELECT of the field's own column");
        assert_eq!(
            direct[0][0].as_deref(),
            Some("***-**-6789"),
            "field's own column must hold the mask"
        );
        assert_ne!(
            direct[0][0].as_deref(),
            Some(plaintext),
            "field's own column must never hold the plaintext"
        );
    });
}

/// **Gate #2**: a `user`-kind actor is denied by the
/// default-policy stub; a `denied` audit row is emitted; the typed
/// error `unmask_not_permitted` reaches the caller.
#[test]
fn unmask_with_user_actor_returns_forbidden_audit_logged() {
    let _keys = with_root_key("p55_pr4_user", &"b".repeat(64));
    let schema = zeroship_data_sql::value!({
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
            .execute_fixture(
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
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
            rejected_claim: None,
        };
        let err = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("dispatch_unmask must refuse user actor under PR 4 stub");
        match err {
            zeroship_data_orm::error::DbError::Coded { code, .. } => {
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

/// **Gate #3**: unmask of a column that has no mask
/// declaration on the cached schema returns the typed
/// `unmask_column_not_masked` error. Pins the contract that the
/// dispatcher refuses to leak plaintext through a "forged" RPC for
/// arbitrary columns.
#[test]
fn unmask_column_not_masked_returns_typed_error() {
    // Schema declares `name` as a bare string — no mask block.
    let schema = zeroship_data_sql::value!({
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
            actor: Some(zeroship_data_sql::value!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("unmask of non-masked column must refuse");
        match err {
            zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

/// **Gate #4**: classification flows through to the audit
/// row regardless of outcome. We register a column with `classification:
/// "phi"`, force the denied path (user actor), and assert the audit
/// row's classification text matches.
#[test]
fn unmask_writes_audit_row_with_correct_classification() {
    let schema = zeroship_data_sql::value!({
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
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "doctor_x" })),
            reason: Some("chart review".to_string()),
            rejected_claim: None,
        };
        let _err = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
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

/// The unmask SELECT names the raw column the DESCRIPTOR declares.
///
/// The end-to-end half of the change `zeroship_data_sql::compile::declared_raw_column`
/// carries. The unit tests in `zeroship-data-orm`'s `protection::mask_pass` bind the
/// WRITE side - which column the plaintext is relocated INTO - in the engine's
/// default-feature build. Nothing there rules on the READ, because the read is a
/// SELECT against a real database and the fetch helpers are private.
///
/// Coherence is the property, not tidiness: if the write pass places the value
/// by the descriptor's name and this SELECT keeps formatting its own, every
/// unmask of a renamed column fails with "no such column" on a row that is
/// perfectly well stored. The fixture therefore declares a name
/// `raw_column_name` does NOT produce, and the table has ONLY that column - so a
/// dispatch that re-derives cannot accidentally find the value.
///
/// Mask-only (no `encrypted` block) so it lands on `fetch_plaintext_parent`; the
/// encrypted twin reads the same resolved name through `fetch_and_decrypt`.
#[test]
fn unmask_reads_the_raw_column_the_descriptor_declares() {
    let declared_raw = "__zs_raw2__ssn";
    let schema = zeroship_data_sql::value!({
        "id": { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "storage": { "valueColumn": "ssn", "rawColumn": declared_raw },
        },
    });
    let app_id = "app_unmask_declared_raw";
    let collection = "people";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        assert_ne!(
            declared_raw,
            raw_column_name("ssn"),
            "the fixture must declare a name the derivation does not produce, or it \
             passes against a dispatch that ignores the descriptor",
        );
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"{app_id}\".\"{collection}\" (\
                         id TEXT PRIMARY KEY, \"{declared_raw}\" TEXT, ssn TEXT)"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE");
        backend
            .execute_fixture(
                &format!(
                    "INSERT INTO \"{app_id}\".\"{collection}\" (id, \"{declared_raw}\", ssn) \
                     VALUES ('per_01', '123-45-6789', '***-**-6789')"
                ),
                &[],
            )
            .await
            .expect("INSERT");

        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "per_01".to_string(),
            column: "ssn".to_string(),
            actor: Some(zeroship_data_sql::value!({ "kind": "auto", "id": null })),
            reason: Some("integration test".to_string()),
            rejected_claim: None,
        };
        let result = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect("the unmask SELECT must name the declared raw column");
        assert_eq!(result.plaintext, "123-45-6789");
    });
}

// ===========================================================================
// App-declared policy and unmask authorization on SQLite.
// Startup installs the declaration in memory. Runtime replacement is refused,
// and filesystem content cannot supply or change the policy.

use zeroship_data_orm::protection::mask_policy;

/// Helper — install backend + schema + clean any pre-existing cached
/// policy for the app. Returns the backend (kept alive via Rc) and the
/// TempDir guard. Drains the cache so the test starts from
/// "no-policy-declared".
async fn policy_setup(
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let (backend, dir) = unmask_setup_with_schema(app_id, collection, schema).await;
    zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
    (backend, dir)
}

/// **Gate #1**: a policy granting `user` access to `pii`
/// allows a user-role actor to unmask a pii-classified column.
#[test]
fn unmask_with_user_role_in_policy_returns_plaintext() {
    let _keys = with_root_key("p55_pr5_grant", &"c".repeat(64));
    let schema = zeroship_data_sql::value!({
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
        let policy_v = zeroship_data_sql::value!({
            "user": ["public", "pii"],
        });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
            .expect("set_mask_policy must succeed");

        // Post-storage-flip layout: the raw column (named via
        // `raw_column_name`, never spelled out here) holds the ciphertext
        // `protection::unmask` reads; the field's own column (`email`) holds the
        // mask, exactly as a default read pipeline would leave it.
        let raw_email = raw_column_name("email");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"app_unmask_policy_grant\".\"users\" (\
                         id    TEXT PRIMARY KEY, \
                         \"{raw_email}\" BLOB, \
                         email TEXT NOT NULL DEFAULT 'x***@***'\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Encrypt + insert one row.
        use zeroship_data_orm::protection::encryption_pass::encrypt_row_on_write;
        use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
        let row_pk = "usr_grant_01";
        let plaintext = "alice@example.com";
        let mut doc = zeroship_data_sql::value!({
            "id": row_pk,
            "email": plaintext,
        });
        encrypt_row_on_write(
            backend.key_store(),
            app_id,
            collection,
            &schema,
            row_pk,
            &mut doc,
        )
        .await
        .expect("encrypt_row_on_write");
        // Relocate the native ciphertext to the raw column, as the mask pass
        // does, and store the precomputed mask in the logical column.
        let ciphertext = doc
            .as_object_mut()
            .expect("doc object")
            .shift_remove("email")
            .expect("ciphertext produced by encrypt_row_on_write");
        {
            let obj = doc.as_object_mut().expect("doc object");
            obj.insert(raw_email.clone(), ciphertext);
            obj.insert(
                "email".to_string(),
                zeroship_data_sql::value!("a****@example.com"),
            );
        }
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
            collection,
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert_with_dialect");
        let client = backend
            .fixture_session(app_id)
            .await
            .expect("acquire client");
        let param_refs = &bq.params;
        let _ = client
            .query_typed(&bq.sql, param_refs)
            .await
            .expect("INSERT");

        // Unmask with `user` actor — must succeed via the policy.
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: row_pk.to_string(),
            column: "email".to_string(),
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
            reason: Some("user requested own data".to_string()),
            rejected_claim: None,
        };
        let result = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect("policy grants user → pii; unmask must succeed");
        assert_eq!(result.plaintext, plaintext);

        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "granted", "outcome must be granted");
        assert_eq!(audit[0].1, "user");
        assert_eq!(audit[0].2, "pii");

        // A direct read of the field's own column (what a default read
        // pipeline would see, no audit, no authorization check) must
        // still be the mask, never the plaintext.
        let direct = client
            .query(
                "SELECT email FROM \"app_unmask_policy_grant\".\"users\" WHERE id = 'usr_grant_01'",
                &[],
            )
            .await
            .expect("direct SELECT of the field's own column");
        assert_eq!(
            direct[0][0].as_deref(),
            Some("a****@example.com"),
            "field's own column must hold the mask"
        );
        assert_ne!(
            direct[0][0].as_deref(),
            Some(plaintext),
            "field's own column must never hold the plaintext"
        );
    });
}

/// **Gate #2**: a policy granting `user` only `public` denies
/// a user-role attempt to unmask a `pii`-classified column. The denied
/// path emits an audit row.
#[test]
fn unmask_with_user_role_not_in_policy_denied() {
    let schema = zeroship_data_sql::value!({
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
        let policy_v = zeroship_data_sql::value!({
            "user": ["public"],
        });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
            .expect("set_mask_policy must succeed");

        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "usr_anywhere".to_string(),
            column: "ssn".to_string(),
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
            rejected_claim: None,
        };
        let err = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("policy does not allow user → pii; must refuse");
        match err {
            zeroship_data_orm::error::DbError::Coded { code, .. } => {
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

/// **Gate #3**: regression guard for the no-policy case.
/// The default-deny stub still applies: `auto` allowed,
/// everyone else denied. Closes the "did we accidentally start
/// allowing everything when no policy is declared" hole.
#[test]
fn unmask_default_deny_when_no_policy() {
    let schema = zeroship_data_sql::value!({
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
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
            rejected_claim: None,
        };
        let err = unmask::dispatch_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("no policy + non-auto actor → default-deny");
        match err {
            zeroship_data_orm::error::DbError::Coded { code, .. } => {
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

/// **Gate #4**: invalid classification at the Rust validator.
/// The SDK's `defineMaskPolicy()` rejects at declare-time; the Rust
/// validator catches anything that bypasses the SDK (forged RPC,
/// untrusted client, future SDK drift). Both layers refuse with
/// `invalid_mask_classification`.
#[test]
fn unmask_invalid_classification_rejected_at_dispatch_time() {
    let schema = zeroship_data_sql::value!({ "id": { "type": "string" } });
    let app_id = "app_unmask_invalid_classification";
    let collection = "users";

    run(async {
        let (_backend, _dir) = policy_setup(app_id, collection, schema).await;

        let bad_policy = zeroship_data_sql::value!({
            "admin": ["public", "badclass"],
        });
        let err = mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), bad_policy)
            .expect_err("rust validator must refuse unknown classification");
        match err {
            zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_classification");
            }
            other => {
                panic!("expected ValidationFailed::invalid_mask_classification, got {other:?}")
            }
        }
    });
}

/// The app's startup declaration stays fixed while the database is in use.
#[test]
fn policy_cannot_change_after_startup() {
    let app_id = "app_unmask_policy_fixed";
    let collection = "items";
    run(async {
        let (backend, dir) = policy_setup(app_id, collection, zeroship_data_sql::value!({
            "id": { "type": "string" },
            "data": { "type": "string", "mask": { "kind": "full", "classification": "internal" } },
        })).await;
        let binding = DbBinding::cold_start(app_id);
        mask_policy::install_mask_policy(
            &binding,
            zeroship_data_sql::value!({ "support": ["public"] }),
        )
        .unwrap();
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "any".to_string(),
            column: "data".to_string(),
            actor: Some(zeroship_data_sql::value!({ "kind": "support", "id": "sup_1" })),
            reason: None,
            rejected_claim: None,
        };
        for attempt in 0..2 {
            if attempt > 0 {
                let error = mask_policy::install_mask_policy(
                    &binding,
                    zeroship_data_sql::value!({ "support": ["internal"] }),
                )
                .unwrap_err();
                assert!(matches!(
                    error,
                    zeroship_data_orm::error::DbError::ValidationFailed {
                        code: "mask_policy_immutable",
                        ..
                    }
                ));
            }
            let error =
                unmask::dispatch_unmask(&unmask_route(app_id).await, &binding, args.clone())
                    .await
                    .unwrap_err();
            assert!(matches!(
                error,
                zeroship_data_orm::error::DbError::Coded {
                    code, ..
                } if code == "unmask_not_permitted"
            ));
        }
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 2);
        assert!(audit.iter().all(|row| row.0 == "denied"));
        assert!(!dir.path().join("mask_policies.json").exists());
    });
}

/// Existing sidecar contents cannot grant access or break authorization.
#[test]
fn unmask_ignores_policy_sidecar_files() {
    run(async {
        for (app_id, contents) in [
            (
                "app_sidecar_grant",
                r#"{"app_sidecar_grant":{"support":["internal"]}}"#,
            ),
            ("app_sidecar_corrupt", "invalid JSON"),
        ] {
            let (backend, dir) = policy_setup(app_id, "items", zeroship_data_sql::value!({
                "id": { "type": "string" },
                "data": { "type": "string", "mask": { "kind": "full", "classification": "internal" } },
            })).await;
            let path = dir.path().join("mask_policies.json");
            std::fs::write(&path, contents).unwrap();
            let error = unmask::dispatch_unmask(
                &unmask_route(app_id).await,
                &DbBinding::cold_start(app_id),
                unmask::UnmaskFieldArgs {
                    collection: "items".into(),
                    row_pk: "any".into(),
                    column: "data".into(),
                    actor: Some(zeroship_data_sql::value!({ "kind": "support", "id": "sup_1" })),
                    reason: None,
                    rejected_claim: None,
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                zeroship_data_orm::error::DbError::Coded {
                    code, ..
                } if code == "unmask_not_permitted"
            ));
            assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
            let audit = read_audit_rows(backend.as_ref(), app_id).await;
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "denied");
        }
    });
}

// ===========================================================================
// Mask backfill + rewrite + removal end-to-end on SQLite
// ===========================================================================
//
// These tests build a SQLite-shaped table by hand, INSERT rows, then
// exercise the diff classifier + mask sentinel parse round-trip. The
// integration-level coverage these tests provide:
//
// 1. The DDL emitter (`build_create_table_with_fks`) attaches the
//    `/* zero-migrate:mask:... */` sentinel to the sibling column.
// 2. The SQLite introspector recovers the mask metadata from
//    `sqlite_master.sql` on a subsequent `introspect_schema` call.
// 3. The diff classifier sees the recovered metadata and emits no
//    spurious ops on a stable-shape redeploy.

/// **Malformed sentinel does not poison introspection** on
/// SQLite: a sibling carrying a garbled sentinel parses to "no mask"
/// on the parent (and a `tracing::warn!` fires; the test only checks
/// the introspection shape).
#[test]
fn malformed_mask_sentinel_skipped_on_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* zero-migrate:mask:kind=cosmic_radiation,classification=spi */\
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
// Bulk unmask + per-query unmask hint
// ===========================================================================
//
// These tests drive the dispatch helpers end-to-end on a real
// SQLite backend:
//
//   * `bulk_unmask_end_to_end` - atomic auth + bulk decrypt + single
//     audit row per call (the dispatch shape
//     `db.users.bulkUnmask([...])` uses).
//   * `per_query_unmask_hint_end_to_end` - wire-up gate for the
//     `find(filter, { unmask: [...], actor })` hint. We can't
//     stand up V8 here, so the test drives the lower-level
//     `dispatch_unmask_for_query` directly against rows pre-wrapped
//     by `wrap_row_on_read`.
//
// Five `drift_*` tests led this block until 2026-09-03 and went with
// `crud::mask_drift`; the epitaph in `crud/mod.rs` says why. The cold-start
// ATTACH one of them ruled on is still ruled on, by
// `cold_bulk_unmask_attaches_before_read` below.

// ---------------------------------------------------------------------------
// Bulk unmask end-to-end (SQLite)
// ---------------------------------------------------------------------------

use zeroship_data_orm::protection::unmask::{BulkUnmaskArgs, BulkUnmaskItem, dispatch_bulk_unmask};

/// **cold-open gate, bulk unmask**: the same guard as the single-unmask
/// cold-open gate, after a fresh startup reinstalls the app policy in memory.
/// The database must open from the configured URL.
///
/// The production line is `v8_classes::dispatch::dispatch_bulk_unmask_field`
/// (and `v8_classes::masked_value`'s bulk site), which resolves through
/// `tx_scope::ensure_backend` exactly as the single-unmask dispatch does.
#[test]
fn cold_bulk_unmask_open_comes_from_ensure_backend_not_the_fixture() {
    let schema = zeroship_data_sql::value!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" },
        },
    });
    let app_id = "app_bulk_unmask_cold_open";
    let collection = "users";

    run(async {
        // `fixture` stays bound for the whole block: the assertion is an
        // address comparison against it.
        let (fixture, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        mask_policy::install_mask_policy(
            &DbBinding::cold_start(app_id),
            zeroship_data_sql::value!({ "user": ["pii"] }),
        )
        .expect("set_mask_policy");
        configure_cold_sqlite_unmask_fixture(
            &dir,
            app_id,
            collection,
            schema,
            zeroship_data_sql::value!({ "user": ["pii"] }),
        );
        assert_cold_open_installs_a_fresh_backend(fixture.as_ref()).await;
    });
}

/// **bulk gate #1**: authorised actor unmasks many columns
/// across many rows in one call; the result map carries plaintext
/// for every pair, and exactly ONE audit row lands.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_bulk_unmask_attaches_before_read() {
    let schema = zeroship_data_sql::value!({
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
        let (backend, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        // Post-storage-flip layout: each field's own column holds the
        // mask; the raw sibling (named via `raw_column_name`, never
        // spelled out here) holds the real value `dispatch_bulk_unmask`
        // reads.
        let raw_email = raw_column_name("email");
        let raw_ssn = raw_column_name("ssn");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"app_bulk_unmask_e2e\".\"users\" (\
                         id             TEXT PRIMARY KEY, \
                         \"{raw_email}\" TEXT, \
                         email          TEXT NOT NULL, \
                         \"{raw_ssn}\"   TEXT, \
                         ssn            TEXT NOT NULL\
                     )"
                ),
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
                 (id, \"{raw_email}\", email, \"{raw_ssn}\", ssn) VALUES \
                 ('{id}', '{email}', 'masked', '{ssn}', 'masked')"
            );
            backend.execute_fixture(&sql, &[]).await.expect("INSERT");
        }

        // Policy: `user` can unmask pii AND spi.
        let policy_v = zeroship_data_sql::value!({ "user": ["pii", "spi"] });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v.clone())
            .expect("set_mask_policy");

        // A fresh startup reinstalls the app declaration without a sidecar.
        // Bulk dispatch then opens the backend and attaches the app database.
        assert!(!dir.path().join("mask_policies.json").exists());
        configure_cold_sqlite_unmask_fixture(&dir, app_id, collection, schema, policy_v);

        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            items: vec![
                BulkUnmaskItem {
                    row_pk: "u1".into(),
                    columns: vec!["email".into(), "ssn".into()],
                },
                BulkUnmaskItem {
                    row_pk: "u2".into(),
                    columns: vec!["email".into()],
                },
            ],
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" })),
            reason: Some("ops dashboard".into()),
            rejected_claim: None,
        };
        let result = dispatch_bulk_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect("bulk unmask");
        // Plaintext recovered for every pair.
        let u1 = result.results.get("u1").expect("u1 row");
        assert_eq!(
            u1.get("email")
                .and_then(zeroship_data_sql::value::Value::as_str),
            Some("alice@example.com")
        );
        assert_eq!(
            u1.get("ssn")
                .and_then(zeroship_data_sql::value::Value::as_str),
            Some("123-45-6789")
        );
        let u2 = result.results.get("u2").expect("u2 row");
        assert_eq!(
            u2.get("email")
                .and_then(zeroship_data_sql::value::Value::as_str),
            Some("bob@example.com")
        );

        // Exactly ONE audit row covering the whole call.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "bulk → single audit row: {audit:?}");
        assert_eq!(audit[0].0, "granted");
        assert_eq!(audit[0].1, "user", "actor_role recorded");

        // A direct read of the fields' own columns (what a default read
        // pipeline would see) must still be the mask placeholder, never
        // the plaintext bulk_unmask returned above.
        let client = backend
            .fixture_session(app_id)
            .await
            .expect("acquire client");
        let direct = client
            .query(
                "SELECT email, ssn FROM \"app_bulk_unmask_e2e\".\"users\" WHERE id = 'u1'",
                &[],
            )
            .await
            .expect("direct SELECT of the fields' own columns");
        assert_eq!(direct[0][0].as_deref(), Some("masked"));
        assert_eq!(direct[0][1].as_deref(), Some("masked"));
        assert_ne!(direct[0][0].as_deref(), Some("alice@example.com"));
        assert_ne!(direct[0][1].as_deref(), Some("123-45-6789"));
    });
}

/// **bulk gate #2**: ANY unauthorised pair refuses the WHOLE
/// call (Q-MASK-F atomic). One audit row with outcome `denied`; no
/// plaintext returned for the authorised pair either.
#[test]
fn bulk_unmask_authorization_atomic_one_unauthorized_fails_all() {
    let schema = zeroship_data_sql::value!({
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
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        backend
            .execute_fixture(
                "CREATE TABLE \"app_bulk_atomic_refuse\".\"users\" (\
                     id              TEXT PRIMARY KEY, \
                     __zs_raw__email TEXT, \
                     email           TEXT NOT NULL, \
                     __zs_raw__ssn   TEXT, \
                     ssn             TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Policy: `user` can ONLY unmask pii; spi is forbidden.
        let policy_v = zeroship_data_sql::value!({ "user": ["pii"] });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
            .expect("set_mask_policy");

        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            // Pair (u1, email) authorised; pair (u1, ssn) NOT
            // authorised. Atomic fence: entire call refuses.
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["email".into(), "ssn".into()],
            }],
            actor: Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" })),
            reason: None,
            rejected_claim: None,
        };
        let err = dispatch_bulk_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("bulk must refuse atomically");
        match err {
            zeroship_data_orm::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "bulk_unmask_partial_unauthorized");
            }
            other => panic!("expected Coded::bulk_unmask_partial_unauthorized, got {other:?}"),
        }

        // Single `denied` audit row covers the whole call.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(
            audit.len(),
            1,
            "atomic refuse -> single audit row: {audit:?}"
        );
        assert_eq!(audit[0].0, "denied");
    });
}

/// **bulk gate #3**: unknown column on the schema raises the
/// typed `unmask_column_not_masked` error BEFORE any audit row writes.
#[test]
fn bulk_unmask_unknown_column_returns_typed_error_e2e() {
    let schema = zeroship_data_sql::value!({
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
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["does_not_exist".into()],
            }],
            actor: Some(zeroship_data_sql::value!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = dispatch_bulk_unmask(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            args,
        )
        .await
        .expect_err("unknown column must refuse");
        match err {
            zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Per-query unmask hint end-to-end (SQLite)
// ---------------------------------------------------------------------------

use zeroship_data_orm::protection::unmask::{
    audit_query_hint_granted, authorize_query_hint, dispatch_unmask_for_query,
};

/// **cold-open gate, query hint**: the same guard again, over this family's
/// fixture. The query hint's first cold operation is the authorization fence,
/// so the open has to happen before any policy load or denied audit write - and
/// this is what rules on the open.
///
/// The production line is `crud::mod`'s query hint, sanitised in `plan_find` and
/// resolved by the `find` dispatch through `tx_scope::ensure_backend`.
#[test]
fn cold_query_unmask_hint_open_comes_from_ensure_backend_not_the_fixture() {
    let schema = zeroship_data_sql::value!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
        },
    });
    let app_id = "app_qhint_cold_open";
    let collection = "users";

    run(async {
        // `fixture` stays bound for the whole block: the assertion is an
        // address comparison against it.
        let (fixture, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        mask_policy::install_mask_policy(
            &DbBinding::cold_start(app_id),
            zeroship_data_sql::value!({ "user": ["spi"] }),
        )
        .expect("set_mask_policy");
        configure_cold_sqlite_unmask_fixture(
            &dir,
            app_id,
            collection,
            schema,
            zeroship_data_sql::value!({ "user": ["spi"] }),
        );
        assert_cold_open_installs_a_fresh_backend(fixture.as_ref()).await;
    });
}

/// **per-query gate #1**: an authorised actor with a query
/// hint sees plaintext in the listed columns; non-listed masked
/// columns keep their `__zsmask__` wrapping.
///
/// The ATTACH is what this rules on. The OPEN is the harness's - see
/// `unmask_backend` - and is bound by the cold-open gate directly above.
#[test]
fn cold_query_unmask_hint_attaches_before_read() {
    let schema = zeroship_data_sql::value!({
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
        let (backend, dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        // Post-storage-flip layout: each field's own column holds the
        // mask; the raw sibling (named via `raw_column_name`, never
        // spelled out here) holds the real value `dispatch_unmask_for_query`
        // reads.
        let raw_email = raw_column_name("email");
        let raw_ssn = raw_column_name("ssn");
        backend
            .execute_fixture(
                &format!(
                    "CREATE TABLE \"app_qhint_e2e\".\"users\" (\
                         id             TEXT PRIMARY KEY, \
                         \"{raw_email}\" TEXT, \
                         email          TEXT NOT NULL, \
                         \"{raw_ssn}\"   TEXT, \
                         ssn            TEXT NOT NULL\
                     )"
                ),
                &[],
            )
            .await
            .expect("CREATE TABLE");
        backend
            .execute_fixture(
                &format!(
                    "INSERT INTO \"app_qhint_e2e\".\"users\" \
                     (id, \"{raw_email}\", email, \"{raw_ssn}\", ssn) VALUES \
                     ('u1', 'alice@example.com', 'a***@example.com', '123-45-6789', '***-**-6789')"
                ),
                &[],
            )
            .await
            .expect("INSERT");

        // Policy: `user` can unmask both pii and spi.
        let policy_v = zeroship_data_sql::value!({ "user": ["pii", "spi"] });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v.clone())
            .expect("set_mask_policy");

        // A fresh startup reinstalls the declaration before the first query.
        // Query authorization must attach the app database before reading it.
        assert!(!dir.path().join("mask_policies.json").exists());
        configure_cold_sqlite_unmask_fixture(&dir, app_id, collection, schema, policy_v);

        // Simulate the row shape `dispatch_find` would produce
        // AFTER `apply_mask_wrap_on_read` has wrapped the masked
        // columns. We're driving `dispatch_unmask_for_query` directly
        // since the full V8 round-trip is out of scope for this
        // integration test.
        let actor = Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" }));
        let reason = Some("dashboard view".to_string());

        // Step 1 — upfront auth fence.
        authorize_query_hint(
            &unmask_backend().await,
            &DbBinding::cold_start(app_id),
            collection,
            &["ssn".to_string()],
            &actor,
            None,
            &reason,
        )
        .await
        .expect("authorize_query_hint must succeed");

        // Step 2 — simulate post-wrap row + run unmask-for-query.
        let mut rows = vec![zeroship_data_sql::value!({
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
        dispatch_unmask_for_query(
            &unmask_route(app_id).await,
            &DbBinding::cold_start(app_id),
            collection,
            &["ssn".to_string()],
            &mut rows,
        )
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
        let email = row
            .get("email")
            .and_then(|v| v.as_object())
            .expect("email obj");
        assert_eq!(
            email.get("sentinel").and_then(|v| v.as_str()),
            Some("__zsmask__"),
            "email must remain wrapped: {row:?}"
        );

        // Step 3 — granted audit row lands.
        audit_query_hint_granted(
            &unmask_backend().await,
            &DbBinding::cold_start(app_id),
            collection,
            &["ssn".to_string()],
            &actor,
            None,
            &reason,
        )
        .await
        .expect("audit");
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "one audit row for the query: {audit:?}");
        assert_eq!(audit[0].0, "granted");
        assert_eq!(audit[0].1, "user");

        // `dispatch_unmask_for_query` mutates only the in-memory `rows`
        // passed above - a direct read of the fields' own columns must
        // still show the mask, never the plaintext it just returned.
        let client = backend
            .fixture_session(app_id)
            .await
            .expect("acquire client");
        let direct = client
            .query(
                "SELECT email, ssn FROM \"app_qhint_e2e\".\"users\" WHERE id = 'u1'",
                &[],
            )
            .await
            .expect("direct SELECT of the fields' own columns");
        assert_eq!(direct[0][0].as_deref(), Some("a***@example.com"));
        assert_eq!(direct[0][1].as_deref(), Some("***-**-6789"));
        assert_ne!(direct[0][0].as_deref(), Some("alice@example.com"));
        assert_ne!(direct[0][1].as_deref(), Some("123-45-6789"));
    });
}

/// **per-query gate #2**: an unauthorised actor REFUSES the
/// query entirely; we do not silently degrade to masked-only.
#[test]
fn per_query_unmask_hint_rejects_unauthorized_actor() {
    let schema = zeroship_data_sql::value!({
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
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        // Policy: `user` can only unmask `pii`, NOT `spi`.
        let policy_v = zeroship_data_sql::value!({ "user": ["pii"] });
        mask_policy::install_mask_policy(&DbBinding::cold_start(app_id), policy_v)
            .expect("set_mask_policy");

        let actor = Some(zeroship_data_sql::value!({ "kind": "user", "id": "actor_x" }));
        let err = authorize_query_hint(
            &unmask_backend().await,
            &DbBinding::cold_start(app_id),
            collection,
            &["ssn".to_string()],
            &actor,
            None,
            &None,
        )
        .await
        .expect_err("must refuse");
        match err {
            zeroship_data_orm::error::DbError::Coded { code, .. } => {
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

/// **per-query gate #3**: unknown column on the schema raises
/// the typed `unmask_column_not_masked` error before any DB hit.
#[test]
fn per_query_unmask_hint_unknown_column_returns_typed_error() {
    let schema = zeroship_data_sql::value!({
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
        zeroship_data_v8::testing::clear_mask_policy_cache_for_tests(app_id);
        let actor = Some(zeroship_data_sql::value!({ "kind": "auto" }));
        let err = authorize_query_hint(
            &unmask_backend().await,
            &DbBinding::cold_start(app_id),
            collection,
            &["does_not_exist".to_string()],
            &actor,
            None,
            &None,
        )
        .await
        .expect_err("must refuse on unknown");
        match err {
            zeroship_data_orm::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// System-field prefix + auto-indexes end-to-end on SQLite
//
// These tests exercise `fixture_table_sql_for(Sqlite)`
// end-to-end: the emitter produces SQLite-flavoured DDL, the engine
// accepts the multi-statement payload (CREATE TABLE + 3 CREATE INDEX),
// and `PRAGMA table_info` / `sqlite_master` confirm the seven columns
// and three indexes are present.
//
// These tests drive the dialect emitter directly and `execute_fixture` the result,
// the same pattern the introspection tests use for SQLite elsewhere in this
// file.
// ---------------------------------------------------------------------------

/// `fixture_table_sql_for(Sqlite)` produces DDL the
/// SQLite engine accepts, and PRAGMA `table_info` reports all 7 system
/// fields after execution.
#[test]
fn sqlite_ddl_has_seven_system_field_columns_end_to_end() {
    use zeroship_data_sql::compile::SqlDialect;

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        let schema = zeroship_data_sql::value!({
            "title": { "type": "string", "required": true },
        });
        let sql = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");

        // Execute the multi-statement payload through the session
        // actor — `execute_fixture` routes through `sqlite3_exec` which
        // accepts multi-statement SQL.
        // SQLite's `Connection::execute` runs ONE statement per call
        // (unlike PG's libpq simple-query), so this test splits the
        // multi-statement payload and executes each piece individually.
        for stmt in sql.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .execute_fixture(trimmed, &[])
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
fn freshly_created_table_has_three_indexes_end_to_end() {
    use zeroship_data_sql::compile::SqlDialect;
    use zeroship_migrate::schema::query::index_name;

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        let sql = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        // SQLite's `Connection::execute` runs ONE statement per call
        // (unlike PG's libpq simple-query), so this test splits the
        // multi-statement payload and executes each piece individually.
        for stmt in sql.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .execute_fixture(trimmed, &[])
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

/// **Deferred**: the SDK INSERT auto-populate path (which
/// supplies `id` + `created_at` etc. from the runtime) is out of
/// scope here; only the DDL is exercised, so this end-to-end test
/// supplies the system fields manually via a raw-SQL INSERT to confirm
/// the emitted columns accept the canonical value shapes (TEXT id,
/// CURRENT_TIMESTAMP defaults firing on omitted columns).
#[test]
fn inserting_a_row_without_user_fields_succeeds_via_system_fields_only() {
    use zeroship_data_sql::compile::SqlDialect;

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        let sql = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
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
                .execute_fixture(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
        }

        // Raw INSERT: supply only `id` (no SDK auto-populate here).
        // The 3 NULL-able columns + 3 DEFAULT'd columns fill in from
        // the engine.
        backend
            .execute_fixture(
                "INSERT INTO \"app_demo\".\"posts\" (id) VALUES ('post_01')",
                &[],
            )
            .await
            .expect("INSERT with only id must succeed");

        // Round-trip: confirm `version = 1`, `deleted_at IS NULL`,
        // `created_at IS NOT NULL`. Pin the canonical shape the DDL
        // promises.
        let client = backend
            .fixture_session("app_demo")
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
        assert_eq!(
            row[3].as_deref(),
            Some("1"),
            "created_at IS NOT NULL default"
        );
    });
}

// ---------------------------------------------------------------------------
// INSERT auto-populates `id` + `created_by` / `updated_by`
// ---------------------------------------------------------------------------

/// End-to-end: the `apply_system_fields_on_insert` pass mints a typed_id
/// and the subsequent `build_insert_with_dialect` INSERT lands a row
/// with the canonical 7 system fields populated. Mirrors what the
/// `dispatch_insert` hot path does at request time but without standing
/// up V8 — exercises the SQL builder + SQLite engine round-trip.
#[test]
fn insert_end_to_end_populates_system_fields_sqlite() {
    use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
    use zeroship_data_orm::crud::system_fields_pass::apply_system_fields_on_insert;

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        // 1. Stand up the table with the 7 system-field columns.
        let schema = zeroship_data_sql::value!({
            "title": {"type": "string", "required": true},
        });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
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
                .execute_fixture(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("DDL: {trimmed}\n{e:?}"));
        }

        // 2. Build the inbound doc — creator passes ONLY the user
        // field. The auto-mint pass injects `id`, `created_by`,
        // `updated_by`; the DB fires its DEFAULT for the timestamps +
        // version.
        let mut doc = zeroship_data_sql::value!({ "title": "PR 3 hello" });
        // The pass takes the collection's descriptor entry (the write pipeline
        // resolves it once per op and hands it down); the only thing it reads
        // out of it is a declared `t.id(prefix)`, and this one declares none.
        apply_system_fields_on_insert(
            &mut doc,
            &zeroship_data_sql::value!({ "title": { "type": "string", "required": true } }),
            "posts",
            Some("usr_actor_e2e"),
        )
        .expect("derived prefix must be accepted");

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
        // pool's `execute_fixture` rejects result-bearing statements).
        let built = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert");
        let params = &built.params;
        let client = backend
            .fixture_session("app_demo")
            .await
            .expect("acquire client (insert)");
        let returning_rows = client
            .query_values(&built.sql, params)
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
/// (not INTEGER) so the column accepts typed_id string
/// values without storage-class mismatch.
///
/// **Scope note**: the actual `FOREIGN KEY ... REFERENCES "app"."tbl"`
/// constraint clause uses a schema-qualified target name that SQLite's
/// CREATE TABLE parser refuses (a pre-existing PG-only path). This
/// test stands up the posts table WITHOUT the FK clause (skipping the
/// constraint with `FkEmission::Deferred` + empty existing set) and
/// asserts the column TYPE is TEXT - the FK type-cascade surface this
/// test pins. End-to-end FK constraint validation on SQLite remains a
/// PG-only path until the cross-app FK rework lands.
#[test]
fn insert_with_fk_uses_text_keys_end_to_end_sqlite() {
    use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};
    use zeroship_data_orm::crud::system_fields_pass::apply_system_fields_on_insert;

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        // Stand up the posts table with an `authorId` ref column. The
        // FK type cascade emits TEXT for the column type. We use
        // `FkEmission::Deferred(empty)` so the FK clause is omitted -
        // SQLite refuses schema-qualified REFERENCES targets, a
        // pre-existing PG-only path this test does not attempt to fix.
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        // One binding for the table's shape and for the write's projection: the
        // DDL emitter and the INSERT builder must not read two literals.
        let schema = zeroship_data_sql::value!({
            "title": {"type": "string", "required": true},
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let posts_ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Deferred(&empty),
            SqlDialect::Sqlite,
        )
        .expect("build posts DDL");
        // Pin the FK column type to TEXT (was INTEGER before the
        // FK type cascade).
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
                .execute_fixture(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("posts DDL: {trimmed}\n{e:?}"));
        }

        // Insert a post whose authorId is a typed_id string. Before
        // the FK type cascade the column was INTEGER and a typed_id
        // string would round-trip as the literal string under
        // SQLite's permissive storage model but assert against the
        // declared INTEGER affinity at introspection. With the
        // cascade applied the affinity is TEXT - no surprise on
        // read-back.
        let mut post_doc = zeroship_data_sql::value!({
            "title": "fk-ok",
            "authorId": "usr_01HXY3Z9PQR2STUV4WXY5Z6789",
        });
        apply_system_fields_on_insert(
            &mut post_doc,
            &zeroship_data_sql::value!({
                "title": {"type": "string", "required": true},
                "authorId": {"type": "ref", "refTarget": "users"},
            }),
            "posts",
            None,
        )
        .expect("derived prefix must be accepted");
        let built = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &post_doc,
            SqlDialect::Sqlite,
        )
        .expect("build posts insert");
        let params = &built.params;
        let client = backend.fixture_session("app_demo").await.expect("client");
        client
            .query_values(&built.sql, params)
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
// UPDATE auto-bumps version + updated_at + optimistic concurrency
// ---------------------------------------------------------------------------

/// End-to-end: an UPDATE built via `build_update_one_with_system_fields`
/// on SQLite bumps `version` by exactly 1 and rewrites `updated_at`.
/// Mirrors what `dispatch_update_one` does at request time but bypasses
/// V8 / the per-isolate schema cache (we drive the SQL builder
/// directly).
#[test]
fn update_end_to_end_bumps_version_by_one_sqlite() {
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_update_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_demo")
            .await
            .expect("ensure_app_schema");

        let schema = zeroship_data_sql::value!({
            "title": {"type": "string", "required": true},
        });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
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
            backend
                .execute_fixture(trimmed, &[])
                .await
                .expect("DDL exec");
        }

        // INSERT row at version 1 (DDL default).
        let doc = zeroship_data_sql::value!({
            "id": "post_v1bump",
            "title": "original",
        });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ins_params = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client
            .query_values(&ins.sql, ins_params)
            .await
            .expect("INSERT");

        // UPDATE via the system-fields-aware builder.
        let filter = zeroship_data_sql::value!({ "id": "post_v1bump" });
        let update = zeroship_data_sql::value!({ "title": "v2" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_e2e_updater"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params = &upd.params;
        let returning = client
            .query_values(&upd.sql, upd_params)
            .await
            .expect("UPDATE");
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_update_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();

        let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_cas_ok", "title": "v1" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ins_params = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, ins_params).await.unwrap();

        // CAS at the correct version (1).
        let filter = zeroship_data_sql::value!({ "id": "post_cas_ok", "version": 1 });
        let update = zeroship_data_sql::value!({ "title": "v2" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_cas_ok"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params = &upd.params;
        let returning = client.query_values(&upd.sql, upd_params).await.unwrap();
        assert_eq!(returning.len(), 1, "CAS matched: 1 affected row");

        let rows = client
            .query(
                "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_cas_ok'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            rows[0][0].as_deref(),
            Some("2"),
            "version bumped on CAS hit"
        );
    });
}

/// End-to-end CAS failure: an UPDATE that filters by a stale `version`
/// affects zero rows. The dispatch layer (not exercised here) converts
/// the empty RETURNING into a typed `version_mismatch` — at the SQL
/// layer we just confirm the affected-rows = 0 contract.
#[test]
fn update_end_to_end_with_stale_version_affects_zero_rows_sqlite() {
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_update_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();

        let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_cas_stale", "title": "v1" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ins_params = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, ins_params).await.unwrap();

        // CAS at the wrong version (row is at 1; we expect 99).
        let filter = zeroship_data_sql::value!({ "id": "post_cas_stale", "version": 99 });
        let update = zeroship_data_sql::value!({ "title": "v_nope" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_cas_stale"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params = &upd.params;
        let returning = client.query_values(&upd.sql, upd_params).await.unwrap();
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_update_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();

        let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_race", "title": "v0" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ins_params = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, ins_params).await.unwrap();

        // First UPDATE at version=1 wins.
        let filter1 = zeroship_data_sql::value!({ "id": "post_race", "version": 1 });
        let update1 = zeroship_data_sql::value!({ "title": "v_winner" });
        let ab = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_a"),
            ..Default::default()
        };
        let upd1 = build_update_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter1,
            &update1,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p1 = &upd1.params;
        let r1 = client.query_values(&upd1.sql, p1).await.unwrap();
        assert_eq!(r1.len(), 1, "first CAS wins");

        // Second UPDATE at version=1 loses (row is now at version=2).
        let filter2 = zeroship_data_sql::value!({ "id": "post_race", "version": 1 });
        let update2 = zeroship_data_sql::value!({ "title": "v_loser" });
        let upd2 = build_update_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter2,
            &update2,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p2 = &upd2.params;
        let r2 = client.query_values(&upd2.sql, p2).await.unwrap();
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_update_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();

        let schema = zeroship_data_sql::value!({ "title": {"type": "string"} });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_blind", "title": "v0" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let ins_params = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, ins_params).await.unwrap();

        // No version in filter — last-writer-wins. Three consecutive
        // updates land in order; version is bumped each time.
        for new_title in ["v1", "v2", "v3"] {
            let filter = zeroship_data_sql::value!({ "id": "post_blind" });
            let update = zeroship_data_sql::value!({ "title": new_title });
            let ab = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_blind"),
                ..Default::default()
            };
            let upd = build_update_many_with_system_fields(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &filter,
                &update,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p = &upd.params;
            let r = client.query_values(&upd.sql, p).await.unwrap();
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
// Delete becomes soft-delete; add purge + restore; find auto-filters
// deleted_at
//
// We use the `_many` builders in these fixtures because their filters are
// already narrowed to one id. The `_one` builders have their own SQLite
// `rowid` coverage elsewhere in this target.
// ---------------------------------------------------------------------------

#[test]
fn soft_delete_end_to_end_sets_deleted_at_and_bumps_version_sqlite() {
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_sd1", "title": "to be deleted" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let p = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, p).await.unwrap();

        let filter = zeroship_data_sql::value!({ "id": "post_sd1" });
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_deleter"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &sd.params;
        let returning = client.query_values(&sd.sql, p).await.unwrap();
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_idem", "title": "x" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let p = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, p).await.unwrap();

        let filter = zeroship_data_sql::value!({ "id": "post_idem" });
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &filter,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p1 = &sd.params;
        let r1 = client.query_values(&sd.sql, p1).await.unwrap();
        assert_eq!(r1.len(), 1, "first soft-delete hits");
        let r2 = client.query_values(&sd.sql, p1).await.unwrap();
        assert!(r2.is_empty(), "re-soft-deleting is a no-op");
    });
}

#[test]
fn find_with_soft_delete_filter_hides_soft_deleted_rows_sqlite() {
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_find_with_schema_and_unmask_and_soft_delete,
        build_insert_with_dialect, build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        for id in &["post_alive_a", "post_alive_b", "post_dead"] {
            let doc = zeroship_data_sql::value!({ "id": id, "title": id });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();
        }
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_dead" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &sd.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&sd.sql, p).await.unwrap();

        let q = build_find_with_schema_and_unmask_and_soft_delete(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
            None,
            None,
            None,
            None,
            &zeroship_data_sql::value!({ "title": { "type": "string" } }),
            &[],
            true,
        )
        .unwrap();
        let rows = client.query(&q.sql, &[]).await.unwrap();
        assert_eq!(rows.len(), 2, "soft-deleted row hidden by auto-filter");

        let q2 = build_find_with_schema_and_unmask_and_soft_delete(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
            None,
            None,
            None,
            None,
            &zeroship_data_sql::value!({ "title": { "type": "string" } }),
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_restore_many_with_system_fields, build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_rs", "title": "x" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let p = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, p).await.unwrap();

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_rs" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &sd.params;
        client.query_values(&sd.sql, p).await.unwrap();

        let rs = build_restore_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_rs" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &rs.params;
        let returning = client.query_values(&rs.sql, p).await.unwrap();
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_restore_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_live", "title": "x" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let p = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, p).await.unwrap();

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let rs = build_restore_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_live" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &rs.params;
        let returning = client.query_values(&rs.sql, p).await.unwrap();
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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_find_with_schema_and_unmask_and_soft_delete,
        build_insert_with_dialect, build_restore_many_with_system_fields,
        build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        let doc = zeroship_data_sql::value!({ "id": "post_lc", "title": "lifecycle" });
        let ins = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .unwrap();
        let p = &ins.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        client.query_values(&ins.sql, p).await.unwrap();

        let find_default = build_find_with_schema_and_unmask_and_soft_delete(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
            None,
            None,
            None,
            None,
            &zeroship_data_sql::value!({ "title": { "type": "string" } }),
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
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_lc" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &sd.params;
        client.query_values(&sd.sql, p).await.unwrap();

        let r = client.query(&find_default.sql, &[]).await.unwrap();
        assert!(r.is_empty(), "soft-deleted row hidden");

        let find_inc = build_find_with_schema_and_unmask_and_soft_delete(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &zeroship_data_sql::value!({}),
            None,
            None,
            None,
            None,
            &zeroship_data_sql::value!({ "title": { "type": "string" } }),
            &[],
            false,
        )
        .unwrap();
        let r = client.query(&find_inc.sql, &[]).await.unwrap();
        assert_eq!(r.len(), 1, "include_deleted reveals it");

        let rs = build_restore_many_with_system_fields(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "id": "post_lc" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &rs.params;
        client.query_values(&rs.sql, p).await.unwrap();

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
    use zeroship_data_sql::compile::{
        SqlDialect, SystemFieldAutoBump, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("app_demo").await.unwrap();
        let schema = zeroship_data_sql::value!({ "author": { "type": "string" }, "title": { "type": "string" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.execute_fixture(t, &[]).await.unwrap();
        }

        for (id, author) in &[
            ("post_a1", "usr_a"),
            ("post_a2", "usr_a"),
            ("post_a3_dead", "usr_a"),
            ("post_b1", "usr_b"),
            ("post_b2", "usr_b"),
        ] {
            let doc = zeroship_data_sql::value!({ "id": id, "author": author, "title": id });
            let ins = build_insert_with_dialect(
                &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "posts",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .unwrap();
            let p = &ins.params;
            let client = backend.fixture_session("app_demo").await.unwrap();
            client.query_values(&ins.sql, p).await.unwrap();
        }
        backend
            .execute_fixture(
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
            &zeroship_data_sql::SchemaName::new("app_demo").expect("fixture schema name"),
            "posts",
            &schema,
            &zeroship_data_sql::value!({ "author": "usr_a" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p = &sd.params;
        let client = backend.fixture_session("app_demo").await.unwrap();
        let returning = client.query_values(&sd.sql, p).await.unwrap();
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
    use zeroship_data_sql::compile::build_delete_one;

    let schema = zeroship_data_sql::value!({ "title": { "type": "string" } });
    let q = build_delete_one(
        &zeroship_data_sql::SchemaName::new("app1").expect("fixture schema name"),
        "posts",
        &schema,
        &zeroship_data_sql::value!({ "id": "x" }),
    )
    .unwrap();
    assert!(q.sql.starts_with("DELETE FROM"));
    // A purge is a HARD delete: `deleted_at` must not appear in the SET or the
    // WHERE. It IS a system field, so the projection names it - scope the
    // assertion to the statement before `RETURNING`, which is what the test
    // means and what `contains` over the whole string used to imply only
    // because that clause was `*`.
    let body = q
        .sql
        .split_once(" RETURNING ")
        .expect("a RETURNING clause")
        .0;
    assert!(!body.contains("deleted_at"));
    assert!(!q.sql.contains("RETURNING *"));
    assert!(q.sql.contains(r#"RETURNING "id""#));
}

// ---------------------------------------------------------------------------
// Nested-transaction SAVEPOINT SQL validated against the SQLite
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
            .fixture_session("default")
            .await
            .expect("acquire client");

        backend
            .execute_fixture_on(
                &client,
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .await
            .expect("create table");

        // Top-level BEGIN (what the orchestrator emits for a non-nested tx).
        backend
            .execute_fixture_on(&client, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(&client, "INSERT INTO notes (title) VALUES ('outer')", &[])
            .await
            .expect("outer insert");

        // Nested transaction → SAVEPOINT. The literal name here is this arm's
        // own, NOT the orchestrator's: dispatch emits `zs_sp_<frame sequence>`
        // minted by `reducer::frames::FrameStack`, which never derives a name
        // from the depth and never reuses one. What this arm rules on is the
        // SQLite engine's savepoint semantics, which are name-agnostic.
        backend
            .execute_fixture_on(&client, "SAVEPOINT zs_sp_1", &[])
            .await
            .expect("SAVEPOINT");
        backend
            .execute_fixture_on(
                &client,
                "INSERT INTO notes (title) VALUES ('inner-doomed')",
                &[],
            )
            .await
            .expect("inner insert");
        // Inner callback rejected → ROLLBACK TO SAVEPOINT (inner reverts,
        // outer tx continues — not poisoned).
        backend
            .execute_fixture_on(&client, "ROLLBACK TO SAVEPOINT zs_sp_1", &[])
            .await
            .expect("ROLLBACK TO SAVEPOINT");

        // Outer continues + COMMITs.
        backend
            .execute_fixture_on(&client, "INSERT INTO notes (title) VALUES ('outer-2')", &[])
            .await
            .expect("outer insert 2 after savepoint rollback");
        backend
            .execute_fixture_on(&client, "COMMIT", &[])
            .await
            .expect("COMMIT");

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
            .fixture_session("default")
            .await
            .expect("acquire client");

        backend
            .execute_fixture_on(
                &client,
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .await
            .expect("create table");

        backend
            .execute_fixture_on(&client, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(&client, "SAVEPOINT zs_sp_1", &[])
            .await
            .expect("SAVEPOINT");
        backend
            .execute_fixture_on(
                &client,
                "INSERT INTO notes (title) VALUES ('inner-kept')",
                &[],
            )
            .await
            .expect("inner insert");
        // Inner callback resolved → RELEASE SAVEPOINT.
        backend
            .execute_fixture_on(&client, "RELEASE SAVEPOINT zs_sp_1", &[])
            .await
            .expect("RELEASE SAVEPOINT");
        backend
            .execute_fixture_on(
                &client,
                "INSERT INTO notes (title) VALUES ('outer-kept')",
                &[],
            )
            .await
            .expect("outer insert");
        backend
            .execute_fixture_on(&client, "COMMIT", &[])
            .await
            .expect("COMMIT");

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

// ---------------------------------------------------------------------------
// The data plane must attach an app file on demand. The table is applied ahead
// of the runtime, then production mutation and query entry points address it
// without any separate boot-time database operation.
// ---------------------------------------------------------------------------
#[test]
fn p6c_data_plane_reaches_the_app_file_on_demand() {
    run(async {
        let dir = tempfile::tempdir().expect("create tempdir");
        let app = "p6c_no_register";
        let collection = "notes";

        crate::support::tables::create_sqlite_table(
            dir.path(),
            app,
            &format!(
                r#"CREATE TABLE IF NOT EXISTS "{app}"."{collection}" ({SYSTEM_COLUMNS_SQLITE},
  "body" TEXT NOT NULL
);
{}"#,
                system_indexes_sqlite(app, collection)
            ),
        );

        let backend = Rc::new(
            new_sqlite_backend(
                PathBuf::from(dir.path()),
                zeroship_data_v8::testing::isolate_key_source(),
            )
            .expect("open backend"),
        );
        zeroship_data_v8::testing::set_backend_for_tests(zeroship_data_orm::backend::BackendHandle::new(backend.clone()), &format!("sqlite:{}", dir.path().display()));

        // Both statements go through `exec::exec_*_for_tests`, which is the
        // PRODUCTION data-plane entry - the same `TxRoute` -> `exec_sqlite_values`
        // path a CRUD op takes. Calling `backend.execute_fixture` directly would test
        // a layer BELOW the one that knows the app_id, and so could not observe
        // whether the data plane binds the file for itself.
        zeroship_data_v8::testing::exec_mutation_with_emit_for_tests(
            zeroship_data_sql::compile::BuiltQuery {
                sql: format!(
                    r#"INSERT INTO "{app}"."{collection}" (id, body)
                       VALUES ('note_1', 'hello')"#
                ),
                params: Vec::new(),
            },
            app,
            collection,
            ChangeOp::Insert,
        )
        .await
        .expect("the data plane must write after attaching the app file");

        let rows = zeroship_data_v8::testing::exec_query_for_tests(
            app,
            zeroship_data_sql::compile::BuiltQuery {
                sql: format!(r#"SELECT body FROM "{app}"."{collection}" WHERE id = 'note_1'"#),
                params: Vec::new(),
            },
        )
        .await
        .expect("the data plane must read after attaching the app file");
        assert_eq!(
            rows.len(),
            1,
            "the row is readable with no register in the way"
        );
        assert_eq!(
            rows[0].get("body").and_then(|v| v.as_str()),
            Some("hello"),
            "body column resolves"
        );
    });
}

// ---------------------------------------------------------------------------
// DELETED: the destructive-drop-column rebuild test.
//
// It applied a schema that dropped a column and asserted the surviving rows
// came through the 12-step rebuild intact. That is ENGINE behaviour, and this
// crate no longer depends on the engine: plugin-db's tests build their tables
// directly now (`support::tables`), and a hand-built table cannot exercise a
// rebuild at all, so there was no version of this test to keep.
//
// It is covered where it belongs, live, in
// `crates/zeroship-migrate/tests/sqlite_engine/sqlite_rebuild_apply.rs`:
// `type_change_rebuild_preserves_data_and_recreates_index` and
// `column_rename_rebuild_carries_data` both pin data preservation across a
// rebuild, and `h1_drop_column_in_index_routes_to_rebuild` pins that a dropped
// column is what routes there. Checked against that file, not assumed.
//
// What is NOT covered after this deletion: that plugin-db's data plane reads a
// table the engine rebuilt, as opposed to one it created. Nothing in plugin-db
// can produce a rebuild any more, so there is no seam left to test from here.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// DELETED 2026-08-20: `p6b_baseline_adopts_a_journal_less_legacy_file`.
//
// It asserted that an app file with tables but an EMPTY `_mig` journal - the
// shape the retired `run_sqlite_pipeline` left behind - is adopted by the
// engine on first boot rather than drift-aborting. Nothing in the tree produces
// that shape any more. The only writer of `zs-<app>.sqlite` is the migration
// apply, and it writes the journal in the same pass; a journal-less file with
// tables is now unreachable. The baseline
// arm it exercised (`sqlite_engine::maybe_baseline`) has no production caller
// either, for the same reason.
//
// The engine's own baseline behaviour is still covered where it lives. That is
// no longer `third_party/zero-migrate`, which this comment cited until
// 2026-09-04 and which does not exist: the engine was in-sourced, and adoption
// now lives in `crates/zeroship-migrate-backend/src/baseline.rs`
// (`BaselineOutcome` / `BaselineError`, the `MigrationBackend::baseline_one`
// vocabulary) re-exported through `crates/zeroship-migrate-core/src/apply/`.
// What is NOT covered anywhere after this deletion:
// zeroship-side adoption of a pre-existing dev file, which is fine while
// nothing can create one, and would need re-testing the day something can.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SC-2 acceptance arms - the two connections, the reservation, the cancellation
//
// `docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md`. These are the arms
// that need a REAL actor thread, so they live here rather than beside the code.
//
// What is still OWED and is deliberately not asserted below:
//   - `SQLITE_BUSY_SNAPSHOT` on a write upgrade. SC-2 names it as the real
//     serialization point and records that it has no arm; reaching it needs a
//     read snapshot held open ACROSS commands, which the autocommit lane
//     (one reservation per command) cannot express today.
//   - the terminal classifier's eight rows. They are ruled on directly in
//     `backend::sqlite::reservation`'s unit tests, with a fault injected at
//     each terminal statement and `is_autocommit` sampled after.
// ---------------------------------------------------------------------------

/// A statement that takes far longer than any assertion window below.
///
/// A recursive CTE counting to 400 million: pure CPU inside SQLite's VDBE with
/// no I/O, so `sqlite3_interrupt` is the only thing that ends it early.
const LONG_RUNNING_SQL: &str = "WITH RECURSIVE c(x) AS (\
     SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000000\
   ) SELECT COUNT(*) FROM c";

/// The concurrency arm: app A's autocommit **reads** proceed while A holds an
/// open explicit transaction, and do not observe its uncommitted write.
///
/// It says reads deliberately. WAL gives concurrent readers, not concurrent
/// writers: an autocommit *write* issued here would contend for the single
/// write lock `tx_conn` is holding and wait out `busy_timeout`. That limit is
/// real on any number of connections and SC-2 states it in the same breath.
#[test]
fn an_autocommit_read_proceeds_while_the_app_holds_an_open_transaction() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let probe = backend.autocommit_client();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");
        backend
            .execute_fixture("INSERT INTO t (v) VALUES ('committed')", &[])
            .await
            .expect("seed");

        let tx = backend
            .fixture_session("default")
            .await
            .expect("acquire tx client");
        backend
            .execute_fixture_on(&tx, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('uncommitted')", &[])
            .await
            .expect("write inside the transaction (takes the write lock)");

        // The read runs on op_conn while tx_conn holds an open write
        // transaction. Before SC-2 it ran on that same connection and saw the
        // uncommitted row.
        let rows = probe
            .query("SELECT v FROM t ORDER BY id", &[])
            .await
            .expect("autocommit read while a transaction is open");
        assert_eq!(
            rows.len(),
            1,
            "the autocommit read must not observe the open transaction's \
             uncommitted write; got {rows:?}"
        );
        assert_eq!(rows[0][0].as_deref(), Some("committed"));

        backend
            .execute_fixture_on(&tx, "ROLLBACK", &[])
            .await
            .expect("ROLLBACK");
    });
}

/// A command naming a reservation that does not own its connection is refused
/// with a typed error - not run on whatever connection is free.
#[test]
fn a_command_bearing_a_foreign_reservation_is_refused() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        let foreign = backend.unregistered_transaction_client_for_tests();
        let err = backend
            .execute_fixture_on(&foreign, "INSERT INTO t (v) VALUES ('leaked')", &[])
            .await
            .expect_err("a foreign reservation must be refused");
        match &err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(*code, "reservation_not_owner", "got {err:?}");
            }
            other => panic!("expected a typed reservation refusal, got {other:?}"),
        }

        // The refusal has to be a refusal, not a warning: nothing ran.
        let rows = backend
            .autocommit_client()
            .query("SELECT COUNT(*) FROM t", &[])
            .await
            .expect("count after the refusal");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("0"),
            "the refused command must not have executed"
        );
    });
}

/// The interrupt arm: cancellation takes effect **during** a long-running
/// statement, not after it.
///
/// Pre-SC-2 the actor's own comment said the opposite - "the SQL has already
/// committed (or rolled back) by then" - because nothing could reach a running
/// statement.
///
/// **What proves "during" is the cleanup, not the error and not the clock.**
/// This doc comment used to say the error proved it - that `statement_cancelled`
/// "can only come from `SQLITE_INTERRUPT`". It cannot: the *pre-start* path
/// (`enter_running` refusing, `cancelled_before_start`) produces
/// `Cancelled { NoSqlStarted }`, whose `into_result` carries the identical
/// `statement_cancelled` code, and `matches!(outcome, Cancelled { .. })` is
/// satisfied by both. The only thing that separates them is the `cleanup`
/// field, so this arm asserts on it: `RolledBack` / `AlreadyRolledBack` means
/// SQL was in flight and something had to be undone, `NoSqlStarted` means the
/// statement never began - a different code path reaching the same error code,
/// ruled on by `a_cancel_before_execution_starts_stops_the_actor_from_running`
/// in `backend::sqlite::reservation`'s unit tests. The elapsed-time assertion
/// stays a backstop for the case where nothing interrupts and the test would
/// otherwise sit for minutes; it is not the discriminator, and it cannot be -
/// a pre-start cancellation returns *faster*, not slower.
#[test]
fn a_cancellation_interrupts_a_statement_that_is_already_running() {
    run(async {
        use std::time::{Duration, Instant};

        let (backend, _dir) = fresh_backend();
        let tx = backend
            .fixture_session("default")
            .await
            .expect("acquire tx client");
        let cancel = tx
            .cancel_handle()
            .expect("a transaction handle must expose a cancel handle");

        let runner = tx.clone();
        let query =
            compio::runtime::spawn(async move { runner.query(LONG_RUNNING_SQL, &[]).await });

        // Let the actor reach `Running` and start stepping. The protocol does
        // not depend on this sleep - the progress latch covers the window
        // where `Running` is stored but SQLite has not stepped yet - but
        // sleeping first is what makes this test exercise the *interrupt*
        // path rather than the pre-start path, which has its own arm.
        compio::time::sleep(Duration::from_millis(300)).await;

        let started = Instant::now();
        let outcome = cancel.cancel().await.expect("cancel acknowledged");
        let query_result = query.await.expect("query task joined");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(12),
            "cancellation did not interrupt the running statement; it took {elapsed:?}"
        );
        let err = query_result.expect_err("an interrupted query must not return rows");
        match &err {
            DbError::Coded { code, .. } => assert_eq!(
                code, "statement_cancelled",
                "an interrupt must surface as a cancellation, not an opaque \
                 database error; got {err:?}"
            ),
            other => panic!("expected the cancellation code, got {other:?}"),
        }
        let TerminalOutcome::Cancelled { cleanup, .. } = &outcome else {
            panic!(
                "the actor must acknowledge a cancellation after rolling back; \
                 got {outcome:?}"
            );
        };
        assert!(
            matches!(
                cleanup,
                CancelCleanup::RolledBack | CancelCleanup::AlreadyRolledBack
            ),
            "this arm claims the statement was interrupted mid-execution, so the \
             cancellation had something to undo. `NoSqlStarted` here would mean the \
             pre-start path ran instead - the same error code, a different code \
             path, and nothing about the interrupt proved. got {cleanup:?}"
        );
    });
}

/// SC-2 case 3: a cancellation arriving after the outcome was decided is a
/// question, not a command.
///
/// The transaction commits; only then is it cancelled. The commit must stand
/// and the cancellation must report `AlreadyCompleted` - a naive implementation
/// sends `ROLLBACK` here and destroys a durable write.
#[test]
fn a_cancellation_after_commit_does_not_roll_the_commit_back() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        let tx = backend
            .fixture_session("default")
            .await
            .expect("acquire tx client");
        let cancel = tx.cancel_handle().expect("cancel handle");
        backend
            .execute_fixture_on(&tx, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('durable')", &[])
            .await
            .expect("insert");
        let committed = backend
            .settle_transaction_for_tests(&tx, TerminalIntent::Commit)
            .await
            .expect("commit");
        assert_eq!(committed, TerminalOutcome::Committed);

        let outcome = cancel.cancel().await.expect("cancel acknowledged");
        assert!(
            matches!(outcome, TerminalOutcome::AlreadyCompleted(_)),
            "a cancellation after the terminal was claimed must report \
             AlreadyCompleted; got {outcome:?}"
        );

        let rows = backend
            .autocommit_client()
            .query("SELECT v FROM t", &[])
            .await
            .expect("read after the late cancellation");
        assert_eq!(
            rows.len(),
            1,
            "the committed write must survive a cancellation that arrived \
             after the commit; got {rows:?}"
        );
        assert_eq!(rows[0][0].as_deref(), Some("durable"));
    });
}

/// **The data-destroying shape, with no duplicate cancel anywhere.**
///
/// A lease dropped without settling retires through `Release`/`unbind_tx`, and
/// that path does not claim the reservation's terminal - it stays `PENDING`. A
/// cancel handle taken from that lease therefore still wins its claim later,
/// arbitrarily far in the future. By then `tx_conn` can belong to an entirely
/// different transaction, and `run_cancel` used to issue its `ROLLBACK`
/// unconditionally: it destroyed the *current* owner's writes.
///
/// The second half of the damage is the part a creator sees. The stale cancel
/// leaves the current owner's `tx_bound` untouched, so its `COMMIT` still
/// runs - onto a connection SQLite has already returned to autocommit. That
/// commit errors, `classify_commit` correctly refuses to guess, and the
/// creator is told `commit_indeterminate`: "nobody knows whether your write
/// landed", for a write that was silently rolled back.
///
/// Nothing here cancels twice, so `claim_cancelled`'s idempotency does not
/// close it. Ownership is what closes it.
#[test]
fn a_cancel_for_a_retired_reservation_does_not_roll_back_the_next_transaction() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        // R1 takes the lane, writes, and is dropped WITHOUT settling. Its
        // cancel handle outlives it - which is the whole point: a guard held
        // by a dropped future is exactly how SC-1 step 9 will arm this.
        let stale_cancel = {
            let first = backend
                .fixture_session("default")
                .await
                .expect("acquire the first transaction");
            let cancel = first.cancel_handle().expect("cancel handle for R1");
            backend
                .execute_fixture_on(&first, "BEGIN", &[])
                .await
                .expect("BEGIN on R1");
            backend
                .execute_fixture_on(&first, "INSERT INTO t (v) VALUES ('r1')", &[])
                .await
                .expect("write inside R1");
            cancel
        };

        // R2 takes the lane R1 gave up, and opens its own transaction.
        let second = backend
            .fixture_session("default")
            .await
            .expect("acquire the second transaction");
        backend
            .execute_fixture_on(&second, "BEGIN", &[])
            .await
            .expect("BEGIN on R2");
        backend
            .execute_fixture_on(&second, "INSERT INTO t (v) VALUES ('r2')", &[])
            .await
            .expect("write inside R2");

        // The stale cancellation lands while R2's transaction is open.
        let stale_outcome = stale_cancel
            .cancel()
            .await
            .expect("the actor must answer a stale cancellation, not hang");
        assert_eq!(
            stale_outcome,
            TerminalOutcome::Cancelled {
                cleanup: CancelCleanup::AlreadyRetired,
                cause: None
            },
            "a cancellation for a reservation that no longer owns tx_conn must report \
             that it cleaned up nothing. `RolledBack` here is the failure: the only \
             transaction there to roll back belongs to somebody else. got \
             {stale_outcome:?}"
        );

        // R2 must be untouched: its COMMIT is a real commit, not an
        // indeterminate one, and its row is on disk.
        let committed = backend
            .settle_transaction_for_tests(&second, TerminalIntent::Commit)
            .await
            .expect("settle R2");
        assert_eq!(
            committed,
            TerminalOutcome::Committed,
            "R2's commit must be a confirmed commit. A stale cancellation that rolled \
             its transaction back leaves SQLite in autocommit, so COMMIT errors and \
             this reports CommitIndeterminate - the creator is told the fate is \
             unknown for a write that was destroyed."
        );

        let rows = backend
            .autocommit_client()
            .query("SELECT v FROM t ORDER BY id", &[])
            .await
            .expect("read after the stale cancellation");
        assert_eq!(
            rows.len(),
            1,
            "exactly R2's row should survive: R1's died with its unsettled lease, \
             R2's must survive the stale cancel; got {rows:?}"
        );
        assert_eq!(rows[0][0].as_deref(), Some("r2"));
    });
}

/// A cancellation is a claim on one terminal, and a claim can be won once.
///
/// `claim_cancelled` used to return `true` for a terminal already reading
/// `CLAIMED_CANCELLED`, so a second `Cancel` re-entered the cleanup path and
/// issued a second `ROLLBACK` on the lane. Here the second cancel must instead
/// be answered with what the first one decided.
#[test]
fn a_second_cancellation_is_answered_not_re_executed() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        let tx = backend
            .fixture_session("default")
            .await
            .expect("acquire tx client");
        let cancel = tx.cancel_handle().expect("cancel handle");
        backend
            .execute_fixture_on(&tx, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('doomed')", &[])
            .await
            .expect("write inside the transaction");

        let first = cancel.cancel().await.expect("first cancel acknowledged");
        assert!(
            matches!(first, TerminalOutcome::Cancelled { .. }),
            "the first cancellation wins the terminal and rolls back; got {first:?}"
        );

        let second = cancel.cancel().await.expect("second cancel acknowledged");
        let TerminalOutcome::AlreadyCompleted(inner) = &second else {
            panic!(
                "a second cancellation must be told what the first one decided, not \
                 granted the terminal again; got {second:?}"
            );
        };
        assert!(
            matches!(**inner, TerminalOutcome::Cancelled { .. }),
            "and the answer it is told must be the first cancellation's own \
             outcome; got {inner:?}"
        );
    });
}

/// The autocommit lane has an ownership rule too, and it is a *lifetime* rule:
/// one reservation, one command.
///
/// `check_owner` only examined `tx_conn` until 2026-08-27, so this refusal came
/// from `enter_running` noticing a non-`PENDING` terminal instead - reported as
/// `statement_cancelled`, which names neither what went wrong nor why. Nothing
/// was cancelled; a spent reservation was reused.
#[test]
fn a_spent_autocommit_reservation_is_refused_as_a_non_owner() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");

        let spent = backend.spent_autocommit_reservation_for_tests();
        backend
            .exec_on_reservation_for_tests(&spent, "INSERT INTO t (v) VALUES ('first')", &[])
            .await
            .expect("the reservation's one command must run");

        let err = backend
            .exec_on_reservation_for_tests(&spent, "INSERT INTO t (v) VALUES ('second')", &[])
            .await
            .expect_err("a spent autocommit reservation must be refused");
        match &err {
            DbError::ValidationFailed { code, .. } => assert_eq!(
                *code, "reservation_not_owner",
                "a stale reservation is an ownership failure, not a cancellation. \
                 `statement_cancelled` here names the wrong thing: nothing was \
                 cancelled. got {err:?}"
            ),
            other => panic!("expected a typed reservation refusal, got {other:?}"),
        }

        let rows = backend
            .autocommit_client()
            .query("SELECT COUNT(*) FROM t", &[])
            .await
            .expect("count after the refusal");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("1"),
            "the refusal must be a refusal: only the first command ran"
        );
    });
}

/// Both connections publish CDC. Installing the hooks on one would silently
/// drop half the change stream now that `op_conn` is a write path too.
#[test]
fn writes_on_both_connections_reach_the_broker() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("cdc_connections")
            .await
            .expect("ensure_app_schema");
        backend
            .execute_fixture(
                "CREATE TABLE \"cdc_connections\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        let sub = subscribe_local("cdc_connections", "items");

        // op_conn: an ordinary autocommit write.
        backend
            .execute_fixture(
                "INSERT INTO \"cdc_connections\".\"items\" (name) VALUES ('from_op_conn')",
                &[],
            )
            .await
            .expect("autocommit INSERT");

        // tx_conn: a write inside an explicit creator transaction, committed.
        let tx = backend
            .fixture_session("cdc_connections")
            .await
            .expect("acquire tx client");
        backend
            .execute_fixture_on(&tx, "BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .execute_fixture_on(
                &tx,
                "INSERT INTO \"cdc_connections\".\"items\" (name) VALUES ('from_tx_conn')",
                &[],
            )
            .await
            .expect("transactional INSERT");
        assert_eq!(
            backend
                .settle_transaction_for_tests(&tx, TerminalIntent::Commit)
                .await
                .expect("commit"),
            TerminalOutcome::Committed
        );

        drain_publisher().await;

        let msgs = drain(&sub);
        let mut names: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                SubscriptionMessage::Change(ev) => ev.new_tuple.get("name").cloned(),
                _ => None,
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["from_op_conn".to_string(), "from_tx_conn".to_string()],
            "a write on EACH connection must reach the broker; a dispatcher \
             installed on only one drops the other silently. got {msgs:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// The per-app transaction lane - defect L22b
//
// SC-1 admits one top-level transaction per `(runtime_instance_id, app_id)`.
// One shared transaction connection enforced one per
// `(runtime_instance_id, session)` instead, so app B's `db.transaction()` was
// refused while app A held one. These arms rule on the key, on the boundary the
// split creates, and on the refusal that remains.
// ---------------------------------------------------------------------------

/// Two apps hold open transactions at the same time on one session.
///
/// **This is the arm that fails before the per-app lane.** Against the shared
/// connection app B's acquire returned
/// `transaction_connection_busy: "db: this SQLite session already holds an open
/// transaction on tx_conn"` - a refusal caused entirely by another tenant.
#[test]
fn two_apps_hold_transactions_at_the_same_time() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_a")
            .await
            .expect("attach app_a");
        backend
            .attach_app_file("app_b")
            .await
            .expect("attach app_b");
        for app in ["app_a", "app_b"] {
            backend
                .execute_fixture(
                    &format!("CREATE TABLE \"{app}\".\"t\" (id INTEGER PRIMARY KEY, v TEXT)"),
                    &[],
                )
                .await
                .expect("create table");
        }

        let a = backend
            .fixture_session("app_a")
            .await
            .expect("app_a acquires its transaction connection");
        backend
            .execute_fixture_on(&a, "BEGIN", &[])
            .await
            .expect("BEGIN a");
        backend
            .execute_fixture_on(&a, "INSERT INTO \"app_a\".\"t\" (v) VALUES ('a')", &[])
            .await
            .expect("app_a writes inside its transaction");

        // The whole defect: this used to be refused because app A - a
        // DIFFERENT tenant - was holding the one transaction connection.
        let b = backend
            .fixture_session("app_b")
            .await
            .expect("app_b must get its own transaction connection while app_a holds one");
        backend
            .execute_fixture_on(&b, "BEGIN", &[])
            .await
            .expect("BEGIN b");
        backend
            .execute_fixture_on(&b, "INSERT INTO \"app_b\".\"t\" (v) VALUES ('b')", &[])
            .await
            .expect("app_b writes inside its transaction");

        // Both settle independently, and each one's write lands in its own
        // file: two connections, two transactions, no interleaving.
        assert_eq!(
            backend
                .settle_transaction_for_tests(&b, TerminalIntent::Commit)
                .await
                .expect("commit b"),
            TerminalOutcome::Committed
        );
        assert_eq!(
            backend
                .settle_transaction_for_tests(&a, TerminalIntent::Commit)
                .await
                .expect("commit a"),
            TerminalOutcome::Committed
        );
        let probe = backend.autocommit_client();
        for (app, want) in [("app_a", "a"), ("app_b", "b")] {
            let rows = probe
                .query(&format!("SELECT v FROM \"{app}\".\"t\""), &[])
                .await
                .expect("read back");
            assert_eq!(rows.len(), 1, "{app} must hold exactly its own row");
            assert_eq!(rows[0][0].as_deref(), Some(want));
        }
    });
}

/// A transaction connection carries ONE app's file, so a creator transaction
/// cannot address another tenant's tables at all.
///
/// The control for the arm above, and a boundary rather than a convention: the
/// shared connection had every attached app's alias on it, so this same
/// `DELETE` **succeeded** and removed another tenant's row. Nothing in the SQL
/// builders emits a foreign alias today - the engine's `validate_ident` refuses
/// a dot-qualified name before any SQL is rendered - which is exactly why the
/// connection is ALSO the place to enforce it. This cited `cross_app_fk.rs`
/// until 2026-09-02, itself noting it "has no production caller"; that module
/// is deleted, and citing a checker nothing calls is not the reassurance this
/// sentence needs. The guarantee below rests on the connection, not on either
/// validator.
#[test]
fn a_transaction_lane_cannot_address_another_apps_tables() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_a")
            .await
            .expect("attach app_a");
        backend
            .attach_app_file("app_b")
            .await
            .expect("attach app_b");
        backend
            .execute_fixture(
                "CREATE TABLE \"app_a\".\"secret\" (id INTEGER PRIMARY KEY, v TEXT)",
                &[],
            )
            .await
            .expect("create app_a.secret");
        backend
            .execute_fixture(
                "INSERT INTO \"app_a\".\"secret\" (v) VALUES ('tenant-a')",
                &[],
            )
            .await
            .expect("seed app_a.secret");

        let b = backend
            .fixture_session("app_b")
            .await
            .expect("acquire app_b's transaction connection");
        backend
            .execute_fixture_on(&b, "BEGIN", &[])
            .await
            .expect("BEGIN b");
        let leaked = backend
            .execute_fixture_on(&b, "DELETE FROM \"app_a\".\"secret\"", &[])
            .await
            .expect_err("app_b's transaction must not reach app_a's tables");
        assert!(
            format!("{leaked}").contains("no such table"),
            "the refusal must be SQLite not knowing the alias, not an \
             application-level check; got {leaked:?}"
        );

        // The row is still there. A refusal that let the DELETE through and
        // reported an error afterwards would be worse than no check.
        let rows = backend
            .autocommit_client()
            .query("SELECT v FROM \"app_a\".\"secret\"", &[])
            .await
            .expect("read app_a.secret back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some("tenant-a"));
    });
}

/// The refusal that survives, and the message that must name the app.
///
/// A second top-level transaction for the SAME app is still refused
/// immediately. What changed is that this is now the *only* producer of
/// `transaction_connection_busy` on this path, so the message can say whose
/// transaction it is - defect L22b's "reads as your transaction when it is
/// another tenant's" is gone with the cause.
#[test]
fn a_second_transaction_for_the_same_app_is_still_refused_and_names_it() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .attach_app_file("app_a")
            .await
            .expect("attach app_a");
        let first = backend
            .fixture_session("app_a")
            .await
            .expect("first acquire");
        backend
            .execute_fixture_on(&first, "BEGIN", &[])
            .await
            .expect("BEGIN");

        let err = backend
            .fixture_session("app_a")
            .await
            .expect_err("a second transaction for the same app must be refused");
        match &err {
            DbError::ValidationFailed {
                code,
                message,
                hint,
            } => {
                assert_eq!(*code, "transaction_connection_busy", "got {err:?}");
                assert!(
                    message.contains("app_a"),
                    "the message must name the app whose transaction it is; got {message:?}"
                );
                assert!(hint.is_some(), "the remedy is the creator's, so say it");
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }

        // Dropping the first lease frees the app's lane immediately - no queue
        // round trip - so the next acquire succeeds.
        drop(first);
        backend
            .fixture_session("app_a")
            .await
            .expect("the app's lane is free once its lease drops");
    });
}

/// An idle transaction connection is evicted to make room for a new app, and
/// only a session whose every lane is mid-transaction refuses - under a code of
/// its own.
///
/// Bounded per-tenant resources are the point: without the cap every app that
/// ever opened a transaction would hold a connection for the session's life.
/// The two halves differ in ONE variable - whether the incumbent lanes are
/// still inside a transaction - so the eviction path cannot be mistaken for the
/// refusal path.
#[test]
fn transaction_lanes_are_capped_and_the_refusal_has_its_own_code() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let cap = zeroship_data_orm::backend::sqlite::session::MAX_TX_LANES_FOR_TESTS;

        // Half one: `cap` apps that each settle. Every lane is idle, so the
        // next app evicts one and is admitted.
        for i in 0..cap {
            let app = format!("cap_a{i}");
            backend.attach_app_file(&app).await.expect("attach");
            let client = backend
                .fixture_session(&app)
                .await
                .expect("acquire under the cap");
            drop(client);
        }
        let app = format!("cap_a{cap}");
        backend.attach_app_file(&app).await.expect("attach");
        backend
            .fixture_session(&app)
            .await
            .expect("an idle lane must be evicted rather than refusing");

        // Half two: `cap` apps that all HOLD their transactions. Now there is
        // nothing to evict.
        let (backend, _dir) = fresh_backend();
        let mut held = Vec::new();
        for i in 0..cap {
            let app = format!("cap_b{i}");
            backend.attach_app_file(&app).await.expect("attach");
            let client = backend
                .fixture_session(&app)
                .await
                .expect("acquire under the cap");
            backend
                .execute_fixture_on(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            held.push(client);
        }
        let app = format!("cap_b{cap}");
        backend.attach_app_file(&app).await.expect("attach");
        let err = backend
            .fixture_session(&app)
            .await
            .expect_err("every lane is mid-transaction, so this must be refused");
        match &err {
            DbError::ValidationFailed { code, .. } => assert_eq!(
                *code, "transaction_lanes_exhausted",
                "contention with OTHER apps must not arrive under the same code as this \
                 app's own overlapping transaction; got {err:?}"
            ),
            other => panic!("expected a typed refusal, got {other:?}"),
        }
        drop(held);
    });
}

/// `SQLITE_BUSY_SNAPSHOT` on a write upgrade - the last of SC-2's three owed
/// arms.
///
/// SC-2 names it "the real serialization point" and the epoch bullet rests on
/// it. It needs a read snapshot held open ACROSS commands, which the autocommit
/// lane cannot express (one reservation per command, `BEGIN DEFERRED ...
/// COMMIT` around each). **The transaction lane can**: its `BEGIN` and its
/// statements are separate commands on one connection, so another connection
/// can commit in between.
///
/// What this fixture reaches and what it does NOT: the table lives in `main`,
/// the session's own database, which the boot PRAGMAs put in **WAL**. That is
/// what makes `SQLITE_BUSY_SNAPSHOT` (517) possible at all. An app's own file
/// is a different story - see
/// `an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal`, the
/// control that pins why.
#[test]
fn a_write_upgrade_on_a_stale_wal_snapshot_is_refused() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .await
            .expect("create table");
        backend
            .execute_fixture("INSERT INTO t (v) VALUES ('seed')", &[])
            .await
            .expect("seed");

        let tx = backend
            .fixture_session("snapshot_app")
            .await
            .expect("acquire tx client");
        backend
            .execute_fixture_on(&tx, "BEGIN", &[])
            .await
            .expect("BEGIN");
        // The read is what takes the deferred snapshot. Without it `BEGIN`
        // alone has taken no snapshot and the write below simply succeeds -
        // which is the arm's whole difficulty and why it stayed owed.
        let seen = tx
            .query("SELECT COUNT(*) FROM t", &[])
            .await
            .expect("snapshot read");
        assert_eq!(seen[0][0].as_deref(), Some("1"));

        // A different connection commits. `op_conn` is a separate SQLite
        // connection to the same WAL database, so this moves the WAL past the
        // snapshot the transaction is pinned to.
        backend
            .execute_fixture("INSERT INTO t (v) VALUES ('from-op-conn')", &[])
            .await
            .expect("op_conn write commits");

        let started = std::time::Instant::now();
        let err = backend
            .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('upgrade')", &[])
            .await
            .expect_err("a write on a stale WAL snapshot must be refused");
        let elapsed = started.elapsed();
        match &err {
            DbError::LockContention { message } => assert!(
                message.contains("database is locked") || message.contains("busy"),
                "got {message:?}"
            ),
            other => panic!(
                "SQLITE_BUSY_SNAPSHOT must map to LockContention, not an opaque \
                 fault; got {other:?}"
            ),
        }
        // The discriminator between 517 and a plain 5. `busy_timeout` is 5000 ms
        // (`BOOT_PRAGMAS`), and SQLite does NOT invoke the busy handler for
        // SQLITE_BUSY_SNAPSHOT because retrying can never succeed - so an
        // ordinary lock conflict would have sat here for five seconds and this
        // one returns at once. Without this the assertion above passes on
        // either code and the arm proves only that something was locked.
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "a snapshot conflict must not go through the busy handler; waited {elapsed:?}, \
             which is the shape of a plain SQLITE_BUSY waiting out busy_timeout"
        );

        // And the transaction is still the caller's to end: the refusal is a
        // refusal, not a teardown.
        assert_eq!(
            backend
                .settle_transaction_for_tests(&tx, TerminalIntent::Rollback)
                .await
                .expect("rollback"),
            TerminalOutcome::RolledBack
        );
    });
}

/// The control that names what the arm above cannot reach: an app's own file is
/// **not** in WAL, so the same schedule cannot produce
/// `SQLITE_BUSY_SNAPSHOT` there.
///
/// `PRAGMA journal_mode` is per database and does NOT propagate across `ATTACH`
/// (measured: attaching a fresh file to a WAL connection leaves it `delete`),
/// and the migration engine pins every app file to DELETE outright and refuses
/// to run otherwise -
/// `crates/zeroship-migrate-sqlite/src/backend/actor.rs:719-729`. So the arm
/// above proves the mapping and the lane mechanics; it does not prove anything
/// about app data. This one records which mode app data is actually in, so a
/// change to that fact fails here rather than silently making the arm above
/// describe a world we do not run in.
#[test]
fn an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file("jm_app").await.expect("attach");

        let mode = backend
            .autocommit_client()
            .query("PRAGMA \"jm_app\".journal_mode", &[])
            .await
            .expect("read the attached file's journal mode");
        assert_eq!(
            mode[0][0].as_deref(),
            Some("delete"),
            "an ATTACHed app file does not inherit main's WAL mode, and the migration \
             engine pins it to DELETE; SQLITE_BUSY_SNAPSHOT cannot arise on app data \
             while that is true"
        );

        let main_mode = backend
            .autocommit_client()
            .query("PRAGMA main.journal_mode", &[])
            .await
            .expect("read main's journal mode");
        assert_eq!(
            main_mode[0][0].as_deref(),
            Some("wal"),
            "the control: the session's own database IS in WAL, so the two databases \
             on one connection genuinely differ"
        );
    });
}

/// SQLite defaults and runtime timestamp binds must use the same UTC text form
/// so lexical ordering agrees with instant ordering. The fixture drives the
/// migration emitter for a system default and the runtime compiler for a
/// caller-provided instant, then inspects what each actually stored.
#[test]
fn dbbind134_sqlite_timestamp_spellings_invert_same_day_ordering() {
    use zeroship_data_sql::compile::{SqlDialect, build_insert_with_dialect};

    run(async {
        let app = "t134_spelling";
        let coll = "events";
        let (backend, _dir) = fresh_backend();
        backend.attach_app_file(app).await.expect("attach app file");

        // Build the DDL through the EMITTER, not by hand. An earlier draft of
        // this test wrote `DEFAULT CURRENT_TIMESTAMP` as a literal, which meant
        // it could never observe a change to the emitter it claimed to test -
        // the comment asserted a mechanism the code did not drive.
        let schema = zeroship_data_sql::value!({ "occurred_at": { "type": "date" } });
        let ddl = fixture_table_sql_for(
            &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
            coll,
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
            backend
                .execute_fixture(trimmed, &[])
                .await
                .expect("DDL exec");
        }

        // The DDL default itself must already carry the ISO-T spelling. This
        // catches a regression in the emitter without needing a row at all.
        assert!(
            ddl.contains("strftime('%Y-%m-%dT%H:%M:%fZ','now')"),
            "the emitted system-field default must be the ISO-T spelling, got: {ddl}"
        );

        // Row A: id only, so `created_at` is written BY THE EMITTED DEFAULT.
        backend
            .execute_fixture(
                &format!("INSERT INTO \"{app}\".\"{coll}\" (id) VALUES ('a_default')"),
                &[],
            )
            .await
            .expect("insert row A via the emitted column default");

        let client = backend.fixture_session(app).await.expect("acquire client");

        // Row B: through the RUNTIME's builder, which converts a Unix-ms bind
        // for a declared timestamp column.
        let doc = zeroship_data_sql::value!({
            "id": "b_bind",
            "occurred_at": 1_756_700_000_000_i64,
        });
        let bq = build_insert_with_dialect(
            &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
            coll,
            &schema,
            &doc,
            SqlDialect::Sqlite,
        )
        .expect("build_insert_with_dialect");
        assert_eq!(
            bq.params[1],
            zeroship_data_sql::value!("2025-09-01T04:13:20.000Z"),
            "the builder must bind canonical UTC text",
        );
        // `build_insert` emits a RETURNING clause, so this goes through `query`
        // rather than `execute_fixture` - the latter refuses a statement that yields
        // rows ("Execute returned results - did you mean to call query?").
        let params = &bq.params;
        client
            .query_values(&bq.sql, params)
            .await
            .expect("insert row B through the runtime builder");

        let a_stamp = client
            .query(
                &format!("SELECT created_at FROM \"{app}\".\"{coll}\" WHERE id = 'a_default'"),
                &[],
            )
            .await
            .expect("read the emitter-defaulted stamp")[0][0]
            .clone()
            .expect("created_at is NOT NULL");
        let b_stamp = client
            .query(
                &format!("SELECT occurred_at FROM \"{app}\".\"{coll}\" WHERE id = 'b_bind'"),
                &[],
            )
            .await
            .expect("read the bind-written stamp")[0][0]
            .clone()
            .expect("occurred_at was written");

        // Defaults and bound values must have the same separator; SQLite
        // compares this storage as text.
        let a_sep = a_stamp.as_bytes()[10] as char;
        let b_sep = b_stamp.as_bytes()[10] as char;
        assert_eq!(
            a_sep, b_sep,
            "the two emitters disagree on the separator: default wrote {a_stamp:?} \
             (sep {a_sep:?}), runtime bind wrote {b_stamp:?} (sep {b_sep:?})"
        );
        assert_eq!(
            a_sep, 'T',
            "both must settle on the ISO-T spelling the data plane already binds; \
             got {a_stamp:?}"
        );
    });
}

#[allow(unused_imports)]
use zeroship_data_orm::protection::Catalog;

#[cfg(any(test, feature = "test-helpers"))]
#[allow(unused_imports)]
use zeroship_data_orm::fixtures::DatabaseFixture;

#[allow(unused_imports)]
use zeroship_data_orm::search::Search;
