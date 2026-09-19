//! SQLite connections contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use std::path::PathBuf;

use std::rc::Rc;

use zeroship_data_orm::backend::sqlite::SqliteBackend;

use zeroship_data_orm::backend_selection::new_sqlite_backend;

use zeroship_data_orm::cdc::ChangeOp;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

async fn pragma_value(backend: &SqliteBackend, pragma: &str) -> String {
    let client = backend
        .fixture_session(&crate::tests::fixtures::harness_alias("default"))
        .await
        .expect("acquire client");
    let sql = format!("PRAGMA {pragma}");
    let rows = client.query(&sql, &[]).await.expect("PRAGMA query");
    assert_eq!(rows.len(), 1, "PRAGMA {pragma} must return exactly one row");
    rows[0][0].clone().unwrap_or_default()
}

#[test]
fn pragma_journal_mode_is_wal_after_open() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let mode = pragma_value(&backend, "journal_mode").await;
            // SQLite returns "wal" (lowercase) from `PRAGMA journal_mode`.
            assert_eq!(
                mode.to_ascii_lowercase(),
                "wal",
                "boot PRAGMA should have set journal_mode = WAL"
            );
        });
    })
}

#[test]
fn pragma_busy_timeout_set() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let timeout = pragma_value(&backend, "busy_timeout").await;
            assert_eq!(
                timeout,
                crate::budgets::DB_LOCK_TIMEOUT_MS.to_string(),
                "every connection waits on a lock for the lock budget"
            );
        });
    })
}

/// A transaction another host keeps open on a rollback-journal file, blocking
/// the switch to WAL: a schema read, or a bootstrap's write transaction.
///
/// The holder seeds the file first. An empty database is read without holding
/// the shared lock, so a read hold on one would contend with nothing.
fn hold_lock(file: &std::path::Path, transaction: &str) -> rusqlite::Connection {
    let holder = rusqlite::Connection::open(file).expect("open the holder");
    holder
        .execute_batch("CREATE TABLE held (x INTEGER); INSERT INTO held VALUES (1);")
        .expect("seed the holder's file");
    holder
        .execute_batch(transaction)
        .expect("take the holder's lock");
    holder
}

const READ_HOLD: &str = "BEGIN; SELECT count(*) FROM held;";
const WRITE_HOLD: &str = "BEGIN IMMEDIATE; INSERT INTO held VALUES (2);";

async fn open_file(
    file: &std::path::Path,
) -> Result<zeroship_data_orm::backend::BackendHandle, crate::error::DbError> {
    crate::ConnectOptions::new(
        format!("sqlite:{}", file.display()),
        crate::encryption::ProjectKeySource::unavailable(),
    )
    .connect()
    .await
}

/// Opening a file whose holder releases it shortly succeeds once the lock is
/// free, instead of refusing at once.
async fn open_waits_for_a_short_hold(transaction: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("contended.sqlite");
    let holder = hold_lock(&file, transaction);
    let hold = std::time::Duration::from_millis(300);
    let started = std::time::Instant::now();
    let release = std::thread::spawn(move || {
        std::thread::sleep(hold);
        holder.execute_batch("COMMIT").expect("release the lock");
    });

    let opened = open_file(&file).await;
    let elapsed = started.elapsed();
    release.join().expect("holder thread");
    let backend = opened.expect("the open must outlast a short hold");
    assert!(
        elapsed >= hold,
        "the open cannot finish before the holder releases; took {elapsed:?}"
    );
    let backend = backend
        .get::<SqliteBackend>()
        .expect("a SQLite URL opens the SQLite backend");
    assert_eq!(pragma_value(backend, "journal_mode").await, "wal");
}

/// The hold SQLite's busy handler already covers: the switch waits for a
/// reader on its own. The one-variable control for the arm below.
#[test]
fn opening_a_file_another_host_reads_waits_for_the_reader() {
    Host::test(|host| host.run(open_waits_for_a_short_hold(READ_HOLD)))
}

/// The hold the busy handler does NOT cover: the switch reads before it
/// writes, and SQLite refuses a read transaction's upgrade at once rather
/// than invoking the handler, so the switch is retried instead.
#[test]
fn opening_a_file_another_host_writes_waits_for_the_writer() {
    Host::test(|host| host.run(open_waits_for_a_short_hold(WRITE_HOLD)))
}

/// The control for the arms above: a hold that outlasts the lock budget is
/// refused with the typed contention error once the budget is spent, not
/// waited on indefinitely.
#[test]
fn opening_a_file_locked_past_the_lock_budget_is_refused() {
    Host::test(|host| {
        host.run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let file = dir.path().join("contended.sqlite");
            let holder = hold_lock(&file, WRITE_HOLD);
            let budget =
                std::time::Duration::from_millis(u64::from(crate::budgets::DB_LOCK_TIMEOUT_MS));
            let started = std::time::Instant::now();

            let refused = open_file(&file).await;
            let elapsed = started.elapsed();
            drop(holder);
            assert!(
                matches!(refused, Err(crate::error::DbError::LockContention { .. })),
                "a hold past the budget must be refused as lock contention: {refused:?}"
            );
            assert!(
                elapsed >= budget,
                "the refusal must come after waiting out the budget; took {elapsed:?}"
            );
        });
    })
}

#[test]
fn execute_fixture_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
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
    })
}

#[test]
fn execute_fixture_on_round_trip() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias("default"))
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
    })
}

#[test]
fn ensure_app_schema_attaches_file() {
    Host::test(|host| {
        host.run(async {
            let (backend, dir) = fresh_backend(host);
            let alias = crate::tests::fixtures::harness_alias("app_demo");
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding_for_alias(&alias))
                .await
                .expect("ensure_app_schema");

            // The per-app file should now exist on disk.
            let expected = dir.path().join(format!("zs-{alias}.sqlite"));
            assert!(
                expected.exists(),
                "per-app sqlite file should exist: {expected:?}"
            );

            // The alias should be queryable. `SELECT name FROM
            // "<alias>".sqlite_master` returns the (empty) catalog of
            // the freshly-attached database — the SELECT itself
            // succeeding is the assertion (a missing alias surfaces as
            // `no such database: <alias>`).
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias("app_demo"))
                .await
                .expect("acquire client");
            let rows = client
                .query(&format!("SELECT name FROM \"{alias}\".sqlite_master"), &[])
                .await
                .expect("query attached sqlite_master");
            // Freshly attached database has no user tables yet.
            assert!(
                rows.is_empty(),
                "freshly attached db should have no sqlite_master rows; got {rows:?}"
            );
        });
    })
}

#[test]
fn ensure_app_schema_idempotent() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            // First call attaches.
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding("app_demo"))
                .await
                .expect("first ensure_app_schema");
            // Second call must NOT surface "database <alias> is already
            // in use" — the cache (or the error-suppression fallback)
            // should short-circuit it to Ok.
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding("app_demo"))
                .await
                .expect("second ensure_app_schema must be idempotent");
        });
    })
}

#[test]
fn ensure_app_schema_isolates_per_app() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let alias_a = crate::tests::fixtures::harness_alias("app_a");
            let alias_b = crate::tests::fixtures::harness_alias("app_b");
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding_for_alias(&alias_a))
                .await
                .expect("attach app_a");
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding_for_alias(&alias_b))
                .await
                .expect("attach app_b");

            // Create a table inside the `app_a` namespace.
            backend
                .execute_fixture(
                    &format!("CREATE TABLE \"{alias_a}\".\"t\" (x INTEGER)"),
                    &[],
                )
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
                    &format!("SELECT name FROM \"{alias_a}\".sqlite_master WHERE type = 'table'"),
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
                    &format!("SELECT name FROM \"{alias_b}\".sqlite_master WHERE type = 'table'"),
                    &[],
                )
                .await
                .expect("query app_b sqlite_master");
            assert!(
                rows_b.is_empty(),
                "app_b must not see app_a's tables; got {rows_b:?}"
            );
        });
    })
}

/// The three system indexes every confined table carries.
fn system_indexes_sqlite(app_id: &str, collection: &str) -> String {
    let alias = crate::tests::fixtures::harness_alias(app_id);
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{alias}"."{collection}_deleted_at_idx" ON "{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{alias}"."{collection}_updated_at_idx" ON "{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{alias}"."{collection}_created_by_idx" ON "{collection}" ("created_by");
"#
    )
}

#[test]
fn p6c_data_plane_reaches_the_app_file_on_demand() {
    Host::test(|host| {
        host.run(async {
            let dir = tempfile::tempdir().expect("create tempdir");
            let app = "p6c_no_register";
            let collection = "notes";
            let alias = crate::tests::fixtures::harness_alias(app);

            crate::tests::fixtures::tables::create_sqlite_table(
                dir.path(),
                &alias,
                &format!(
                    r#"CREATE TABLE IF NOT EXISTS "{alias}"."{collection}" ({SYSTEM_COLUMNS_SQLITE},
  "body" TEXT NOT NULL
);
{}"#,
                    system_indexes_sqlite(app, collection)
                ),
            );

            let backend = Rc::new(
                new_sqlite_backend(PathBuf::from(dir.path()), host.key_source())
                    .expect("open backend"),
            );
            host.install_backend(
                zeroship_data_orm::backend::BackendHandle::new(backend.clone()),
                &format!("sqlite:{}", dir.path().display()),
            );

            // Both statements go through `exec::exec_*_for_tests`, which is the
            // PRODUCTION data-plane entry - the same `TxRoute` -> `exec_sqlite_values`
            // path a CRUD op takes. Calling `backend.execute_fixture` directly would test
            // a layer BELOW the one that knows the app_id, and so could not observe
            // whether the data plane binds the file for itself.
            crate::tests::fixtures::cache_schema(
                app,
                collection,
                crate::value!({"body":{"type":"string"}}),
            );
            host.exec_mutation_with_emit(
                crate::sql::compiler::CompiledQuery {
                    sql: format!(
                        r#"INSERT INTO "{alias}"."{collection}" (id, body)
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

            let rows = host
                .exec_query(
                    app,
                    crate::sql::compiler::CompiledQuery {
                        sql: format!(
                            r#"SELECT body FROM "{alias}"."{collection}" WHERE id = 'note_1'"#
                        ),
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
    })
}

/// The seven system columns and the `["id"]` primary key, SQLite spelling.
const SYSTEM_COLUMNS_SQLITE: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TEXT NULL"#;
