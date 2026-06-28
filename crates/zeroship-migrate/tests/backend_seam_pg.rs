//! Seam-boundary tests for the `MigrationBackend` trait (SQLite-parity design
//! §2.0/§2.1, M3 — P1).
//!
//! These pin the **trait boundary itself**, not just the end-to-end apply: they
//! drive [`PostgresBackend`] *through the `MigrationBackend` trait* and assert
//! each seam method dispatches to the real Postgres behavior it replaced. This
//! gives the extraction direct coverage so a future refactor of the façade (or
//! the eventual SQLite sibling) has a contract to hold to — in particular the M3
//! moves that were previously inline in the generic body:
//!
//! - `validate_non_txn` (was a raw `pg_query::parse` at executor.rs:490),
//! - `snapshot_schema` (drift introspection over `information_schema`),
//! - the journal row I/O and the lock/session leaves.
//!
//! Requires `zeroship_migrate_test` on :5440 (same harness as `executor_pg`).

use compio_postgres::Client;
use zeroship_migrate::apply::backend::{MigrationBackend, PostgresBackend};
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::{
    apply, Approval, ExecutorConfig, Migration, MigrationFlags, MigrationId,
};
use zeroship_schema::query::SqlDialect;

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput {
            up,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

fn mig_nontxn(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.flags.transactional = false;
    m.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m));
    m
}

/// The PG backend reports the Postgres dialect through the seam.
#[compio::test]
async fn pg_backend_reports_postgres_dialect() {
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);
    assert_eq!(backend.dialect(), SqlDialect::Postgres);
}

/// Selector pin for ARCH P2a: PG has transactional DDL, so the backend-owned
/// two-phase selector reduces exactly to the migration flag.
#[compio::test]
async fn pg_backend_transactional_ddl_selector_matches_migration_flag() {
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);
    let transactional = mig(
        MigrationId::generate(),
        "default_txn",
        "CREATE TABLE selector_default (id bigint)",
    );
    let non_transactional = mig_nontxn(
        MigrationId::generate(),
        "nontxn",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS selector_idx ON selector_default (id)",
    );

    assert!(backend.ddl_is_transactional());
    assert!(!backend.uses_two_phase_path(&transactional));
    assert!(backend.uses_two_phase_path(&non_transactional));
}

/// `ensure_journal` + `applied` round-trip THROUGH THE TRAIT: a fresh meta schema
/// has an empty net-state, and the journal bootstrap is the one the seam drives.
#[compio::test]
async fn pg_backend_ensure_journal_and_applied_through_trait() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    ensure_project_schema(&conn, &cfg).await;
    let backend = PostgresBackend::new(&conn);

    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure_journal via seam");
    let entries = backend.applied(&cfg).await.expect("applied via seam");
    assert!(entries.is_empty(), "fresh journal is empty net-state");

    drop_schemas(&conn, &cfg).await;
}

/// The project lock acquire/release leaves are reachable and idempotent through
/// the seam (a release of an unheld lock is a no-op on PG, not an error path we
/// must guard — acquire then release is the contract).
#[compio::test]
async fn pg_backend_lock_acquire_release_through_trait() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    let backend = PostgresBackend::new(&conn);

    backend
        .acquire_project_lock(&cfg)
        .await
        .expect("acquire via seam");
    backend
        .release_project_lock(&cfg)
        .await
        .expect("release via seam");
}

/// Session snapshot/restore round-trips through the seam: mutate a GUC, restore
/// the captured snapshot, and confirm the original value is back. This pins that
/// the seam's snapshot/restore are the real `current_setting`/`set_config` pair.
#[compio::test]
async fn pg_backend_session_snapshot_restore_through_trait() {
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);

    // Capture the baseline, then mutate statement_timeout on the session.
    let snap = backend.snapshot_session().await.expect("snapshot via seam");
    conn.batch_execute("SET statement_timeout = 12345")
        .await
        .expect("mutate guc");
    let mutated: String = conn
        .query_one("SELECT current_setting('statement_timeout') AS v", &[])
        .await
        .expect("read mutated")
        .get("v");
    assert_eq!(mutated, "12345ms");

    // Restore through the seam — the original value must come back.
    backend.restore_session(&snap).await.expect("restore via seam");
    let restored: String = conn
        .query_one("SELECT current_setting('statement_timeout') AS v", &[])
        .await
        .expect("read restored")
        .get("v");
    assert_eq!(
        restored, snap.statement_timeout,
        "restore_session via the seam returns the captured GUC"
    );
}

/// **M3 extraction pin:** `validate_non_txn` is the seam method that replaced the
/// inline `pg_query::parse` in `apply_locked`. The PG impl must still REJECT a
/// `CREATE INDEX CONCURRENTLY` that lacks `IF NOT EXISTS` (non-idempotent on the
/// crash-recovery re-run) and ACCEPT the `IF NOT EXISTS` form.
#[compio::test]
async fn pg_backend_validate_non_txn_through_trait() {
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);

    let bad = mig_nontxn(
        MigrationId::generate(),
        "bad",
        "CREATE INDEX CONCURRENTLY idx_x ON t (c)",
    );
    let err = backend
        .validate_non_txn(&bad)
        .expect_err("non-idempotent CONCURRENTLY must be rejected via the seam");
    let msg = format!("{err}");
    assert!(
        msg.contains("IF NOT EXISTS") || msg.to_lowercase().contains("idempotent"),
        "rejection cites the missing IF NOT EXISTS: {msg}"
    );

    let good = mig_nontxn(
        MigrationId::generate(),
        "good",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_x ON t (c)",
    );
    backend
        .validate_non_txn(&good)
        .expect("idempotent CONCURRENTLY accepted via the seam");
}

/// **M3 extraction pin:** `snapshot_schema` is the drift-introspection seam. After
/// applying a table it must surface that table (over `information_schema`), proving
/// the seam dispatches to the real PG introspection.
#[compio::test]
async fn pg_backend_snapshot_schema_through_trait() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    ensure_project_schema(&conn, &cfg).await;

    // Create a table directly in the project schema (the introspection target).
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".widgets (id integer primary key, label text)",
        cfg.project_schema
    ))
    .await
    .expect("create widgets");

    let backend = PostgresBackend::new(&conn);
    let snap = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot_schema via seam");
    let table = snap
        .tables
        .get("widgets")
        .expect("widgets present in the seam snapshot");
    let cols: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(cols.contains(&"id") && cols.contains(&"label"), "columns: {cols:?}");

    drop_schemas(&conn, &cfg).await;
}

/// End-to-end through the seam: the public `apply` (which constructs a
/// `PostgresBackend` internally) drives a real migration, and reading the journal
/// back THROUGH THE TRAIT shows the version net-applied. This pins that the seam
/// `apply` path and the seam `applied` read agree on net-state.
#[compio::test]
async fn apply_then_read_journal_through_trait_agree() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    ensure_project_schema(&conn, &cfg).await;

    let v = MigrationId::generate();
    let m = mig(
        v.clone(),
        "create_things",
        &format!(
            "CREATE TABLE \"{}\".things (id integer primary key)",
            cfg.project_schema
        ),
    );

    let outcome = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
        .await
        .expect("apply via public entry (constructs PostgresBackend)");
    assert_eq!(outcome.applied, vec![v.as_str().to_string()]);

    let backend = PostgresBackend::new(&conn);
    let entries = backend.applied(&cfg).await.expect("applied via seam");
    assert!(
        entries.iter().any(|e| e.version == v.as_str()),
        "the applied version is net-applied when read through the seam"
    );

    drop_schemas(&conn, &cfg).await;
}
