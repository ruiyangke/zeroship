//! Faithful executor + journal tests against a REAL Postgres (no shims).
//!
//! Requires a dedicated database `zeroship_migrate_test` on :5440 (recreated by
//! the test runbook). Set `MIGRATE_TEST_DB` to override the DSN; otherwise the
//! tests connect to the dedicated DB and skip (printing a notice) only if the
//! DSN env is explicitly unset AND the default is unreachable.
//!
//! Each test runs in its **own meta + project schema** (suffixed by a unique
//! token) so the shared database stays clean across tests and a re-run is
//! independent.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::{
    apply, ensure_journal, executor::ApplyError, journal, ExecutorConfig, Migration,
    MigrationFlags, MigrationId,
};
use zeroship_migrate::migration::Checksum;

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

/// A unique token so each test gets isolated schemas in the shared DB.
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
    c.meta_schema = format!("meta_{tok}");
    c
}

/// Create the project schema the migrations will populate (the platform would
/// provision this when the project is created; tests do it explicitly).
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
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

/// Build a transactional migration with a correct checksum.
fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(up, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
    }
}

/// Build a non-transactional migration.
fn mig_nontxn(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.flags.transactional = false;
    m
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence");
    !rows.is_empty()
}

async fn journal_count(conn: &Client, cfg: &ExecutorConfig) -> i64 {
    let entries = journal::applied(conn, cfg).await.expect("read journal");
    i64::try_from(entries.len()).unwrap()
}

// ---------------------------------------------------------------------------
// Journal tests (§2.2)
// ---------------------------------------------------------------------------

#[compio::test]
async fn journal_bootstrap_creates_schema_and_table() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;

    ensure_journal(&conn, &cfg).await.expect("ensure_journal");

    assert!(
        table_exists(&conn, &cfg.meta_schema, "schema_migrations").await,
        "journal table must exist after bootstrap"
    );
    assert!(
        table_exists(&conn, &cfg.meta_schema, "schema_migrations_inflight").await,
        "inflight side-table must exist after bootstrap"
    );
    assert_eq!(journal_count(&conn, &cfg).await, 0, "fresh journal is empty");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn journal_bootstrap_is_idempotent() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;

    ensure_journal(&conn, &cfg).await.expect("first bootstrap");
    // Seed a row so we can prove re-bootstrap does not wipe it.
    journal::record_completed(
        &conn, &cfg, "mig_keepit", "keep", "deadbeef", "actor", 1,
    )
    .await
    .expect("seed row");
    ensure_journal(&conn, &cfg).await.expect("second bootstrap");
    ensure_journal(&conn, &cfg).await.expect("third bootstrap");

    assert_eq!(
        journal_count(&conn, &cfg).await,
        1,
        "re-bootstrap must preserve existing journal rows"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn journal_immutability_trigger_rejects_update_and_delete() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    journal::record_completed(&conn, &cfg, "mig_immut", "n", "csum", "actor", 5)
        .await
        .expect("insert row");

    let upd = conn
        .batch_execute(&format!(
            "UPDATE \"{}\".schema_migrations SET name = 'x' WHERE version = 'mig_immut'",
            cfg.meta_schema
        ))
        .await;
    assert!(upd.is_err(), "UPDATE must be rejected by the immutability trigger");

    let del = conn
        .batch_execute(&format!(
            "DELETE FROM \"{}\".schema_migrations WHERE version = 'mig_immut'",
            cfg.meta_schema
        ))
        .await;
    assert!(del.is_err(), "DELETE must be rejected by the immutability trigger");

    // Row is still there.
    assert_eq!(journal_count(&conn, &cfg).await, 1);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Executor apply tests (§2.3)
// ---------------------------------------------------------------------------

#[compio::test]
async fn apply_creates_table_and_records_journal_row() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m = mig(
        MigrationId::generate(),
        "create_widgets",
        &format!(
            "CREATE TABLE \"{}\".widgets (id bigint primary key)",
            cfg.project_schema
        ),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect("apply");

    assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
    assert!(out.recovered.is_empty());
    assert!(
        table_exists(&conn, &cfg.project_schema, "widgets").await,
        "the CREATE TABLE must have run"
    );
    assert_eq!(journal_count(&conn, &cfg).await, 1, "one journal row recorded");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn apply_is_idempotent_on_rerun() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m = mig(
        MigrationId::generate(),
        "create_gadgets",
        &format!(
            "CREATE TABLE \"{}\".gadgets (id bigint primary key)",
            cfg.project_schema
        ),
    );
    let set = [m];
    let first = apply(&conn, &cfg, &set, "actor").await.expect("first apply");
    assert_eq!(first.applied.len(), 1);

    let second = apply(&conn, &cfg, &set, "actor").await.expect("re-apply");
    assert!(second.is_noop(), "re-run with same set is a no-op");
    assert!(second.applied.is_empty());
    assert_eq!(journal_count(&conn, &cfg).await, 1, "no double-journal");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn apply_guard_gate_blocks_dangerous_up_and_runs_nothing() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // COPY ... TO PROGRAM is shell RCE — must be denied by the guard.
    let m = mig(
        MigrationId::generate(),
        "evil_copy",
        &format!(
            "COPY \"{}\".widgets TO PROGRAM 'sh -c \"id\"'",
            cfg.project_schema
        ),
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect_err("guard must abort the apply");
    assert!(
        matches!(err, ApplyError::Guard { .. }),
        "expected Guard error, got {err:?}"
    );

    // The journal exists (bootstrapped) but the migration NEVER ran: no row.
    assert_eq!(
        journal_count(&conn, &cfg).await,
        0,
        "denied migration must not be journaled"
    );
    assert!(
        !table_exists(&conn, &cfg.project_schema, "widgets").await,
        "denied migration must not have created anything"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn apply_failing_sql_rolls_back_with_no_partial_ddl_or_journal() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // First statement is valid, second references a non-existent column —
    // the whole txn must roll back, leaving the first CREATE TABLE undone.
    let up = format!(
        "CREATE TABLE \"{s}\".half_built (id bigint primary key); \
         ALTER TABLE \"{s}\".half_built ADD CONSTRAINT c CHECK (nonexistent > 0);",
        s = cfg.project_schema
    );
    let m = mig(MigrationId::generate(), "bad_sql", &up);

    let err = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect_err("bad SQL must fail");
    assert!(
        matches!(err, ApplyError::MigrationFailed { .. }),
        "expected MigrationFailed, got {err:?}"
    );

    assert!(
        !table_exists(&conn, &cfg.project_schema, "half_built").await,
        "the failed txn must roll back the CREATE TABLE"
    );
    assert_eq!(
        journal_count(&conn, &cfg).await,
        0,
        "a failed migration must not be journaled"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn apply_aborts_on_checksum_drift() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m = mig(
        MigrationId::generate(),
        "create_things",
        &format!(
            "CREATE TABLE \"{}\".things (id bigint primary key)",
            cfg.project_schema
        ),
    );
    apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect("initial apply");

    // Tamper: present the SAME version with a DIFFERENT checksum/up. The drift
    // check must hard-abort before applying anything new.
    let mut tampered = m.clone();
    tampered.up = format!(
        "CREATE TABLE \"{}\".things (id bigint primary key, extra text)",
        cfg.project_schema
    );
    tampered.checksum = Checksum::of(&tampered.up, None);

    let err = apply(&conn, &cfg, std::slice::from_ref(&tampered), "actor")
        .await
        .expect_err("checksum drift must abort");
    assert!(
        matches!(err, ApplyError::ChecksumDrift { .. }),
        "expected ChecksumDrift, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn concurrent_apply_serializes_via_advisory_lock_no_double_apply() {
    // Two independent sessions apply the SAME migration set for the SAME
    // project concurrently. The project advisory lock must serialize them: one
    // applies, the other waits then sees it applied and no-ops. Exactly one
    // journal row, exactly one "applied" across the two outcomes.
    let conn_a = pg().await;
    let conn_b = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn_a, &cfg).await;
    ensure_project_schema(&conn_a, &cfg).await;

    let m = mig(
        MigrationId::generate(),
        "concurrent_table",
        &format!(
            "CREATE TABLE \"{}\".concurrent_t (id bigint primary key)",
            cfg.project_schema
        ),
    );
    let set_a = [m.clone()];
    let set_b = [m];
    let cfg_a = cfg.clone();
    let cfg_b = cfg.clone();

    let ta = compio::runtime::spawn(async move {
        apply(&conn_a, &cfg_a, &set_a, "actor-a").await
    });
    let tb = compio::runtime::spawn(async move {
        apply(&conn_b, &cfg_b, &set_b, "actor-b").await
    });
    let out_a = ta.await.expect("join a").expect("apply a");
    let out_b = tb.await.expect("join b").expect("apply b");

    let total_applied = out_a.applied.len() + out_b.applied.len();
    assert_eq!(
        total_applied, 1,
        "exactly one of the two concurrent applies should apply the migration; \
         a={out_a:?} b={out_b:?}"
    );

    // Verify exactly one journal row via a fresh connection.
    let checker = pg().await;
    assert_eq!(
        journal_count(&checker, &cfg).await,
        1,
        "advisory lock must prevent double-apply"
    );

    drop_schemas(&checker, &cfg).await;
}

#[compio::test]
async fn non_transactional_concurrently_applies_two_phase() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // Seed a table to index.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".items (id bigint primary key, label text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_items_label",
        &format!(
            "CREATE INDEX CONCURRENTLY idx_items_label ON \"{}\".items (label)",
            cfg.project_schema
        ),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect("non-txn apply");
    assert_eq!(out.applied.len(), 1);
    assert!(out.recovered.is_empty(), "first apply is not a recovery");

    // Index exists and is valid; journal has one completed row, no inflight.
    let valid: Vec<bool> = conn
        .query(
            "SELECT x.indisvalid FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = 'idx_items_label'",
            &[&cfg.project_schema],
        )
        .await
        .expect("index query")
        .into_iter()
        .map(|r| r.get::<_, bool>("indisvalid"))
        .collect();
    assert_eq!(valid, vec![true], "index must be built and valid");
    assert_eq!(journal_count(&conn, &cfg).await, 1);

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn non_transactional_recovers_from_crashed_started_marker() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".orders (id bigint primary key, sku text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_orders_sku",
        &format!(
            "CREATE INDEX CONCURRENTLY idx_orders_sku ON \"{}\".orders (sku)",
            cfg.project_schema
        ),
    );

    // Simulate a crash mid-CONCURRENTLY: an INVALID index residue + a lone
    // `started` marker, with NO completed journal row.
    conn.batch_execute(&format!(
        "CREATE INDEX idx_orders_sku ON \"{}\".orders (sku);",
        cfg.project_schema
    ))
    .await
    .expect("create index to mark invalid");
    // Force it INVALID like an interrupted concurrent build.
    conn.batch_execute(&format!(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid = '\"{}\".idx_orders_sku'::regclass",
        cfg.project_schema
    ))
    .await
    .expect("mark index invalid");
    journal::record_started(
        &conn,
        &cfg,
        m.version.as_str(),
        &m.name,
        m.checksum.as_str(),
        "actor",
    )
    .await
    .expect("seed started marker");

    // Re-run: recovery must drop the INVALID index, re-run CONCURRENTLY, and
    // record completed — idempotently.
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect("recovery apply");
    assert_eq!(out.applied.len(), 1);
    assert_eq!(
        out.recovered,
        vec![m.version.as_str().to_string()],
        "the started-only marker must trigger the recovery path"
    );

    // Exactly one VALID index, one completed journal row, no inflight marker.
    let valid: Vec<bool> = conn
        .query(
            "SELECT x.indisvalid FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = 'idx_orders_sku'",
            &[&cfg.project_schema],
        )
        .await
        .expect("index query")
        .into_iter()
        .map(|r| r.get::<_, bool>("indisvalid"))
        .collect();
    assert_eq!(valid, vec![true], "recovery must leave one valid index");
    assert_eq!(journal_count(&conn, &cfg).await, 1, "one completed row");
    let inflight = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations_inflight",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("inflight query");
    assert!(inflight.is_empty(), "inflight marker must be cleared");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn statement_timeout_aborts_long_migration_cleanly() {
    let conn = pg().await;
    let tok = token();
    let mut cfg = cfg_for(&tok);
    // Tiny timeout so a 5s sleep trips it fast.
    cfg.statement_timeout = Duration::from_millis(250);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // A SELECT pg_sleep up that exceeds the statement_timeout.
    let m = mig(
        MigrationId::generate(),
        "slow_migration",
        "SELECT pg_sleep(5)",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), "actor")
        .await
        .expect_err("must time out");
    assert!(
        matches!(err, ApplyError::MigrationFailed { .. }),
        "statement_timeout must surface as MigrationFailed, got {err:?}"
    );
    assert_eq!(
        journal_count(&conn, &cfg).await,
        0,
        "timed-out migration must not be journaled"
    );

    drop_schemas(&conn, &cfg).await;
}
