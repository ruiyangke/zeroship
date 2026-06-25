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
    apply, ensure_journal, executor::ApplyError, journal, Approval, ExecutorConfig, Migration,
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
    c.pg.meta_schema = format!("meta_{tok}");
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
            cfg.project_schema, cfg.pg.meta_schema
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

/// Build a non-transactional migration.
fn mig_nontxn(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.flags.transactional = false;
    m.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m));
    m
}

async fn index_count(conn: &Client, schema: &str, index: &str) -> i64 {
    let rows = conn
        .query(
            "SELECT 1 FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND x.indisvalid = true",
            &[&schema, &index],
        )
        .await
        .expect("index count");
    i64::try_from(rows.len()).unwrap()
}

async fn show_guc(conn: &Client, name: &str) -> String {
    conn.query_one("SELECT current_setting($1) AS v", &[&name])
        .await
        .expect("show guc")
        .get::<_, String>("v")
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
        table_exists(&conn, &cfg.pg.meta_schema, "schema_migrations").await,
        "journal table must exist after bootstrap"
    );
    assert!(
        table_exists(&conn, &cfg.pg.meta_schema, "schema_migrations_inflight").await,
        "inflight side-table must exist after bootstrap"
    );
    assert_eq!(journal_count(&conn, &cfg).await, 0, "fresh journal is empty");

    drop_schemas(&conn, &cfg).await;
}

/// "go native seq": the journal is ONE consolidated events table whose NATIVE
/// auto-increment PK provides the total order — there is NO standalone
/// `schema_migrations_event_seq` SEQUENCE, NO separate `schema_migrations_rolled_back`
/// table, and applied/rolled_back coexist in one table discriminated by
/// `event_kind`. Net-state ("latest event per version wins") and the immutability of
/// the consolidated table are preserved.
#[compio::test]
async fn journal_is_one_events_table_with_native_pk_order() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("ensure_journal");

    let meta = &cfg.pg.meta_schema;

    // No standalone event_seq SEQUENCE object exists (it was folded into the
    // events table's GENERATED ALWAYS AS IDENTITY PK).
    let seq_count: i64 = conn
        .query_one(
            "SELECT count(*)::bigint AS c FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE c.relkind = 'S' AND n.nspname = $1
                AND c.relname = 'schema_migrations_event_seq'",
            &[&meta.as_str()],
        )
        .await
        .expect("introspect sequences")
        .get("c");
    assert_eq!(seq_count, 0, "no standalone schema_migrations_event_seq sequence must exist");

    // No separate rolled_back table (it folded into schema_migrations).
    assert!(
        !table_exists(&conn, meta, "schema_migrations_rolled_back").await,
        "the separate rolled_back table must NOT exist (folded into schema_migrations)"
    );

    // The events table's event_seq is a true IDENTITY column.
    let is_identity: String = conn
        .query_one(
            "SELECT is_identity FROM information_schema.columns
              WHERE table_schema = $1 AND table_name = 'schema_migrations'
                AND column_name = 'event_seq'",
            &[&meta.as_str()],
        )
        .await
        .expect("introspect identity")
        .get("is_identity");
    assert_eq!(is_identity, "YES", "event_seq must be a GENERATED IDENTITY column");

    // Apply (applied) then rollback (rolled_back) coexist in ONE table; net-state is
    // the latest event per version. Apply v: an `applied` event; then a `rolled_back`
    // event — net state = pending (no completed entry).
    let v = MigrationId::generate();
    journal::record_completed(
        &conn,
        &cfg,
        journal::CompletedRecord {
            version: v.as_str(),
            name: "n",
            checksum: "c1",
            applied_by: "actor",
            exec_ms: 1,
            kind: "apply",
        },
    )
    .await
    .expect("applied event");
    // After the applied event, net-state has v as completed.
    let net = journal::applied(&conn, &cfg).await.expect("applied");
    assert!(
        net.iter().any(|e| e.version == v.as_str() && e.phase == journal::Phase::Completed),
        "v is net-applied after the applied event"
    );

    journal::record_rolled_back(&conn, &cfg, v.as_str(), "n", "c1", "actor", 2)
        .await
        .expect("rolled_back event");
    // The latest event (rolled_back) wins → v is NOT net-applied.
    let net = journal::applied(&conn, &cfg).await.expect("applied after rollback");
    assert!(
        !net.iter().any(|e| e.version == v.as_str() && e.phase == journal::Phase::Completed),
        "the latest event (rolled_back) wins → v is pending again (net-state preserved)"
    );

    // Both events live in the ONE table, ordered by the native PK.
    let total: i64 = conn
        .query_one(
            &format!("SELECT count(*)::bigint AS c FROM \"{meta}\".schema_migrations"),
            &[],
        )
        .await
        .expect("count events")
        .get("c");
    assert_eq!(total, 2, "both the applied and rolled_back events live in one table");

    let kinds: Vec<String> = conn
        .query(
            &format!(
                "SELECT event_kind FROM \"{meta}\".schema_migrations ORDER BY event_seq"
            ),
            &[],
        )
        .await
        .expect("read event_kinds")
        .iter()
        .map(|r| r.get::<_, String>("event_kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["applied".to_string(), "rolled_back".to_string()],
        "native PK order = applied then rolled_back"
    );

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
        &conn,
        &cfg,
        journal::CompletedRecord {
            version: "mig_keepit",
            name: "keep",
            checksum: "deadbeef",
            applied_by: "actor",
            exec_ms: 1,
            kind: "apply",
        },
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

    journal::record_completed(
        &conn,
        &cfg,
        journal::CompletedRecord {
            version: "mig_immut",
            name: "n",
            checksum: "csum",
            applied_by: "actor",
            exec_ms: 5,
            kind: "apply",
        },
    )
    .await
    .expect("insert row");

    let upd = conn
        .batch_execute(&format!(
            "UPDATE \"{}\".schema_migrations SET name = 'x' WHERE version = 'mig_immut'",
            cfg.pg.meta_schema
        ))
        .await;
    assert!(upd.is_err(), "UPDATE must be rejected by the immutability trigger");

    let del = conn
        .batch_execute(&format!(
            "DELETE FROM \"{}\".schema_migrations WHERE version = 'mig_immut'",
            cfg.pg.meta_schema
        ))
        .await;
    assert!(del.is_err(), "DELETE must be rejected by the immutability trigger");

    // Row is still there.
    assert_eq!(journal_count(&conn, &cfg).await, 1);

    drop_schemas(&conn, &cfg).await;
}

/// Regression (T9, finding `journal-truncate-not-blocked`): a row-level
/// `BEFORE UPDATE OR DELETE` trigger does NOT fire on `TRUNCATE`, so without a
/// dedicated statement-level `BEFORE TRUNCATE` trigger, `TRUNCATE` would
/// silently wipe the append-only journal. Assert TRUNCATE is rejected on BOTH
/// append-only tables (the consolidated schema_migrations events table +
/// _supersedes), and that the seeded row survives.
#[compio::test]
async fn journal_immutability_trigger_rejects_truncate() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    journal::record_completed(
        &conn,
        &cfg,
        journal::CompletedRecord {
            version: "mig_trunc",
            name: "n",
            checksum: "csum",
            applied_by: "actor",
            exec_ms: 5,
            kind: "apply",
        },
    )
    .await
    .expect("insert row");

    for tbl in ["schema_migrations", "schema_migrations_supersedes"] {
        let trunc = conn
            .batch_execute(&format!("TRUNCATE \"{}\".{tbl}", cfg.pg.meta_schema))
            .await;
        assert!(
            trunc.is_err(),
            "TRUNCATE of append-only table {tbl} must be rejected by the immutability trigger"
        );
    }

    // The seeded journal row survived the (rejected) TRUNCATEs.
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
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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
    let first = apply(&conn, &cfg, &set, Approval::None, "actor").await.expect("first apply");
    assert_eq!(first.applied.len(), 1);

    let second = apply(&conn, &cfg, &set, Approval::None, "actor").await.expect("re-apply");
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
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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

    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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
    apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("initial apply");

    // Tamper: present the SAME version with a DIFFERENT checksum/up. The drift
    // check must hard-abort before applying anything new.
    let mut tampered = m.clone();
    tampered.up = format!(
        "CREATE TABLE \"{}\".things (id bigint primary key, extra text)",
        cfg.project_schema
    );
    tampered.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&tampered));

    let err = apply(&conn, &cfg, std::slice::from_ref(&tampered), Approval::None, "actor")
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
        apply(&conn_a, &cfg_a, &set_a, Approval::None, "actor-a").await
    });
    let tb = compio::runtime::spawn(async move {
        apply(&conn_b, &cfg_b, &set_b, Approval::None, "actor-b").await
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
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_items_label ON \"{}\".items (label)",
            cfg.project_schema
        ),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_orders_sku ON \"{}\".orders (sku)",
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
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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
                cfg.pg.meta_schema
            ),
            &[],
        )
        .await
        .expect("inflight query");
    assert!(inflight.is_empty(), "inflight marker must be cleared");

    drop_schemas(&conn, &cfg).await;
}

// M2: a non-txn double-crash must NOT lose the inflight marker. On the recovery
// path, `recover_non_transactional` clears the `started` marker, then the `<up>`
// re-runs. If that re-run itself crashes BEFORE `record_completed`, a SECOND
// recovery attempt would observe `had_inflight = false` (the marker is gone) and
// treat the next attempt as a FRESH apply — skipping the INVALID-index cleanup.
// The fix re-writes a `started` marker AFTER recovery and BEFORE the re-run, so a
// re-crash re-enters recovery. We exercise the re-arm faithfully: seed a lone
// `started` marker (had_inflight), then drive a non-txn `up` that FAILS on its
// re-run (its target table does not exist). After the failed apply the inflight
// marker MUST still be present — proof it was re-armed. Pre-fix it would be gone
// (recovery cleared it and nothing re-wrote it), so a re-crash would mis-recover.
#[compio::test]
async fn m2_non_txn_recovery_rearms_inflight_marker_before_rerun() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    // A non-txn migration whose `up` REFERENCES A NONEXISTENT table, so its re-run
    // after recovery FAILS (relation does not exist) — standing in for a crash
    // before `completed`. It is still idempotency-valid (`IF NOT EXISTS` present).
    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_missing",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_missing \
             ON \"{}\".does_not_exist (col)",
            cfg.project_schema
        ),
    );

    // Seed a lone `started` marker (no completed row) → the apply takes the
    // recovery path (`had_inflight = true`).
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

    // The apply MUST fail (the re-run references a nonexistent table).
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("the failing re-run must surface as an error");
    assert!(
        matches!(err, ApplyError::MigrationFailed { .. }),
        "expected a MigrationFailed from the nonexistent-table up, got: {err:?}"
    );

    // THE M2 PROPERTY: the inflight marker for this version must STILL be present
    // (re-armed after recovery), so a re-crash re-enters the recovery path. Pre-fix
    // recovery cleared it and nothing re-wrote it, so this row would be ABSENT.
    let inflight = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations_inflight WHERE version = $1",
                cfg.pg.meta_schema
            ),
            &[&m.version.as_str()],
        )
        .await
        .expect("inflight query");
    assert!(
        !inflight.is_empty(),
        "after a recovery whose re-run failed, the inflight marker MUST be re-armed \
         (M2) so a second crash re-enters recovery; pre-fix it was lost"
    );
    // And no COMPLETED row was journaled (the up failed). We query the completed
    // table directly — `journal::applied` deliberately surfaces the lone `started`
    // inflight marker as an entry too, so it cannot distinguish a re-armed marker
    // from a completed row.
    let completed = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations WHERE version = $1",
                cfg.pg.meta_schema
            ),
            &[&m.version.as_str()],
        )
        .await
        .expect("completed query");
    assert!(
        completed.is_empty(),
        "the failed up must not journal a completed row"
    );

    drop_schemas(&conn, &cfg).await;
}

/// v1.x scope fix: recovery of a crashed non-txn migration must drop ONLY the
/// index its `up` names — an UNRELATED invalid index elsewhere in the project
/// schema (e.g. a human's manual CONCURRENTLY build in progress) must survive.
#[compio::test]
async fn recovery_does_not_drop_unrelated_invalid_index() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".orders (id bigint primary key, sku text, label text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    // The migration we are recovering: its `up` names idx_orders_sku.
    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_orders_sku",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_orders_sku ON \"{}\".orders (sku)",
            cfg.project_schema
        ),
    );

    // Its own crashed residue: an INVALID idx_orders_sku + a lone started marker.
    conn.batch_execute(&format!(
        "CREATE INDEX idx_orders_sku ON \"{}\".orders (sku);",
        cfg.project_schema
    ))
    .await
    .expect("create own index");
    // An UNRELATED invalid index NOT named by m.up — simulates a manual
    // CONCURRENTLY build a human is running elsewhere in the schema.
    conn.batch_execute(&format!(
        "CREATE INDEX idx_orders_label_manual ON \"{}\".orders (label);",
        cfg.project_schema
    ))
    .await
    .expect("create unrelated index");
    conn.batch_execute(&format!(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid IN ('\"{schema}\".idx_orders_sku'::regclass, \
                              '\"{schema}\".idx_orders_label_manual'::regclass)",
        schema = cfg.project_schema
    ))
    .await
    .expect("mark both invalid");
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

    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("recovery apply");
    assert_eq!(
        out.recovered,
        vec![m.version.as_str().to_string()],
        "recovery path must run"
    );

    // The migration's own index is rebuilt + valid.
    assert_eq!(
        index_count(&conn, &cfg.project_schema, "idx_orders_sku").await,
        1,
        "the recovered migration's own index must be valid"
    );
    // The UNRELATED invalid index must STILL EXIST (recovery scoped, did not
    // drop it). It is still INVALID (recovery didn't touch it), so query by name.
    let unrelated_still_present: i64 = conn
        .query_one(
            "SELECT count(*) AS n FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = 'idx_orders_label_manual'",
            &[&cfg.project_schema],
        )
        .await
        .expect("unrelated index query")
        .get("n");
    assert_eq!(
        unrelated_still_present, 1,
        "recovery of idx_orders_sku must NOT drop the unrelated idx_orders_label_manual"
    );

    drop_schemas(&conn, &cfg).await;
}

/// v1.x: the executor honors `depends_on` topologically, even when it inverts
/// version order. The EARLIER-version migration depends on (and references a
/// table created by) the LATER-version one, so pure version order would fail; the
/// topo order applies the depended-on migration first and both succeed.
#[compio::test]
async fn apply_honors_depends_on_over_version_order() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let earlier = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let later = MigrationId::generate();
    assert!(later.as_str() > earlier.as_str());

    // earlier-version migration ADD COLUMN to a table created by the later one,
    // and declares depends_on = [later]. If applied in version order it would
    // fail ("relation does not exist"); topo order must run `later` first.
    let mut m_earlier = mig(
        earlier.clone(),
        "add_col_referencing_later",
        &format!(
            "ALTER TABLE \"{}\".parent ADD COLUMN note text",
            cfg.project_schema
        ),
    );
    m_earlier.depends_on = vec![later.clone()];
    m_earlier.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m_earlier));
    let m_later = mig(
        later.clone(),
        "create_parent",
        &format!("CREATE TABLE \"{}\".parent (id bigint)", cfg.project_schema),
    );

    // Supplied in version order (earlier first) — the executor must reorder.
    let out = apply(&conn, &cfg, &[m_earlier.clone(), m_later.clone()], Approval::None, "actor")
        .await
        .expect("apply honoring depends_on");
    // applied order must be [later, earlier].
    assert_eq!(
        out.applied,
        vec![later.as_str().to_string(), earlier.as_str().to_string()],
        "the depended-on (later-version) migration must apply first"
    );
    assert!(table_exists(&conn, &cfg.project_schema, "parent").await);

    drop_schemas(&conn, &cfg).await;
}

/// v1.x: a `depends_on` cycle is a clear hard error, and nothing is applied.
#[compio::test]
async fn apply_rejects_dependency_cycle() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let a = MigrationId::generate();
    std::thread::sleep(Duration::from_millis(2));
    let b = MigrationId::generate();
    let mut ma = mig(a.clone(), "a", &format!("CREATE TABLE \"{}\".ta ()", cfg.project_schema));
    let mut mb = mig(b.clone(), "b", &format!("CREATE TABLE \"{}\".tb ()", cfg.project_schema));
    ma.depends_on = vec![b.clone()];
    ma.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&ma));
    mb.depends_on = vec![a.clone()];
    mb.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&mb));

    let err = apply(&conn, &cfg, &[ma, mb], Approval::None, "actor").await.unwrap_err();
    assert!(matches!(err, ApplyError::DependencyCycle(_)), "got {err:?}");
    // Nothing applied — neither table exists.
    assert!(!table_exists(&conn, &cfg.project_schema, "ta").await);
    assert!(!table_exists(&conn, &cfg.project_schema, "tb").await);
    assert_eq!(journal_count(&conn, &cfg).await, 0);

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn statement_timeout_aborts_long_migration_cleanly() {
    let conn = pg().await;
    let tok = token();
    let mut cfg = cfg_for(&tok);
    // Tiny timeout so a 5s sleep trips it fast.
    cfg.pg.statement_timeout = Duration::from_millis(250);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // A SELECT pg_sleep up that exceeds the statement_timeout.
    let m = mig(
        MigrationId::generate(),
        "slow_migration",
        "SELECT pg_sleep(5)",
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
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

// ---------------------------------------------------------------------------
// C1 — non-txn recovery is idempotent for a VALID index + lone started marker.
//
// A prior run created the index successfully (it is VALID), then crashed before
// writing the `completed` row, leaving a lone `started` marker. The old recovery
// only dropped INVALID indexes then blindly re-ran the up — a valid index +
// `CREATE INDEX CONCURRENTLY` (without IF NOT EXISTS) would error `already
// exists`. With idempotent non-txn ups (IF NOT EXISTS) + re-run recovery, this
// completes cleanly.
// ---------------------------------------------------------------------------
#[compio::test]
async fn c1_non_txn_recovery_idempotent_for_valid_index_and_lone_started_marker() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".accounts (id bigint primary key, email text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    // Idempotent non-txn up (IF NOT EXISTS) — required by C2 validation.
    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_accounts_email",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_accounts_email ON \"{}\".accounts (email)",
            cfg.project_schema
        ),
    );

    // Simulate success-then-crash: the index already exists and is VALID, and a
    // lone `started` marker is present with NO completed row.
    conn.batch_execute(&format!(
        "CREATE INDEX idx_accounts_email ON \"{}\".accounts (email)",
        cfg.project_schema
    ))
    .await
    .expect("create valid index");
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

    // Recovery must NOT error on the already-existing valid index.
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("recovery must complete idempotently, not error 'already exists'");
    assert_eq!(out.applied.len(), 1);
    assert_eq!(out.recovered, vec![m.version.as_str().to_string()]);

    assert_eq!(
        index_count(&conn, &cfg.project_schema, "idx_accounts_email").await,
        1,
        "exactly one valid index after recovery"
    );
    assert_eq!(journal_count(&conn, &cfg).await, 1, "one completed row");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// C2 — completed-crash for `ALTER TYPE … ADD VALUE IF NOT EXISTS` recovers.
//
// The enum value was already added (op succeeded) but `completed` crashed,
// leaving a lone `started` marker. Re-running a non-IF-NOT-EXISTS ADD VALUE
// would error `label already exists`; the IF NOT EXISTS form re-runs cleanly.
// ---------------------------------------------------------------------------
#[compio::test]
async fn c2_non_txn_recovery_idempotent_for_alter_type_add_value() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    conn.batch_execute(&format!(
        "CREATE TYPE \"{}\".mood AS ENUM ('happy', 'sad')",
        cfg.project_schema
    ))
    .await
    .expect("seed enum");

    let m = mig_nontxn(
        MigrationId::generate(),
        "add_mood_excited",
        &format!(
            "ALTER TYPE \"{}\".mood ADD VALUE IF NOT EXISTS 'excited'",
            cfg.project_schema
        ),
    );

    // Simulate success-then-crash: value already added, lone `started` marker.
    conn.batch_execute(&format!(
        "ALTER TYPE \"{}\".mood ADD VALUE 'excited'",
        cfg.project_schema
    ))
    .await
    .expect("add enum value");
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

    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("recovery must complete idempotently, not 'label already exists'");
    assert_eq!(out.applied.len(), 1);
    assert_eq!(out.recovered, vec![m.version.as_str().to_string()]);
    assert_eq!(journal_count(&conn, &cfg).await, 1, "one completed row");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// C2 validation — a non-txn CREATE INDEX CONCURRENTLY WITHOUT IF NOT EXISTS is
// rejected at apply with a clear error, before any execution.
// ---------------------------------------------------------------------------
#[compio::test]
async fn non_txn_create_index_concurrently_without_if_not_exists_is_rejected() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".people (id bigint primary key, name text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_people_name",
        &format!(
            "CREATE INDEX CONCURRENTLY idx_people_name ON \"{}\".people (name)",
            cfg.project_schema
        ),
    );
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("non-idempotent non-txn up must be rejected");
    assert!(
        matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
        "expected NonIdempotentNonTxn, got {err:?}"
    );

    // Nothing ran, nothing journaled.
    assert_eq!(
        index_count(&conn, &cfg.project_schema, "idx_people_name").await,
        0,
        "rejected migration must not create the index"
    );
    assert_eq!(journal_count(&conn, &cfg).await, 0, "no journal row");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// REGRESSION (nontxn-dml-recovery-double-apply): a bare-DML `up` forced onto the
// non-txn two-phase path (`transaction:false`) is REJECTED at apply, before any
// execution. Pre-fix the guard admitted DML, the loader routed it onto the
// non-txn path, and `recover_non_transactional` re-ran the `up` VERBATIM — so a
// success-then-crash (op committed, `completed` row did not) DOUBLE-APPLIED the
// INSERT on recovery. This test proves (a) the up is refused outright, and (b)
// the double-apply scenario the refusal forecloses: had the engine NOT rejected
// it, a seeded started-marker recovery would have re-inserted the row.
// ---------------------------------------------------------------------------
#[compio::test]
async fn nontxn_bare_dml_is_rejected_and_recovery_cannot_double_apply() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    ensure_journal(&conn, &cfg).await.expect("bootstrap");

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".counters (id bigint primary key, n bigint not null)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    // A pure-DML `up` forced non-transactional via `transaction:false`.
    let m = mig_nontxn(
        MigrationId::generate(),
        "seed_counter",
        &format!(
            "INSERT INTO \"{}\".counters (id, n) VALUES (1, 1)",
            cfg.project_schema
        ),
    );

    // (a) Apply must REJECT it before any execution — no row inserted, no journal.
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("bare-DML non-txn up must be rejected, not routed onto the recovery path");
    assert!(
        matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
        "expected NonIdempotentNonTxn, got {err:?}"
    );
    let n_rows: i64 = conn
        .query_one(
            &format!("SELECT count(*) AS c FROM \"{}\".counters", cfg.project_schema),
            &[],
        )
        .await
        .expect("count")
        .get("c");
    assert_eq!(n_rows, 0, "rejected migration must not insert");
    assert_eq!(journal_count(&conn, &cfg).await, 0, "no journal row");

    // (b) Demonstrate the double-apply the rejection forecloses. Simulate the
    // success-then-crash the recovery path handles for legitimate (idempotent)
    // non-txn ops: the INSERT already ran, a lone `started` marker exists, no
    // `completed`. The OLD behavior would re-run `<up>` verbatim on recovery and
    // insert a SECOND row (here a PK violation — the friendlier face of the same
    // double-apply, which for a non-PK INSERT would silently duplicate). The fix
    // refuses the migration up front, so this corrupting recovery is unreachable.
    conn.batch_execute(&format!(
        "INSERT INTO \"{}\".counters (id, n) VALUES (1, 1)",
        cfg.project_schema
    ))
    .await
    .expect("simulate the committed (pre-crash) insert");
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

    // Re-apply still REJECTS at validation (before touching the started marker),
    // so the row count stays at exactly one — recovery never re-runs the INSERT.
    let err2 = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("recovery of a bare-DML non-txn up must still be refused, not re-run");
    assert!(matches!(err2, ApplyError::NonIdempotentNonTxn { .. }), "got {err2:?}");
    let n_after: i64 = conn
        .query_one(
            &format!("SELECT count(*) AS c FROM \"{}\".counters", cfg.project_schema),
            &[],
        )
        .await
        .expect("count")
        .get("c");
    assert_eq!(n_after, 1, "recovery must not double-apply the INSERT");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// H1 — a mid-batch guard denial applies NOTHING (static guard is all-up-front).
// ---------------------------------------------------------------------------
#[compio::test]
async fn h1_guard_denial_in_batch_applies_nothing() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // A safe migration ORDERED BEFORE a denied one. The denied one must abort
    // the whole batch before the safe one commits.
    let v_safe = MigrationId::generate();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let v_denied = MigrationId::generate();
    assert!(v_safe.as_str() < v_denied.as_str(), "safe must sort first");

    let safe = mig(
        v_safe.clone(),
        "safe_table",
        &format!(
            "CREATE TABLE \"{}\".safe_t (id bigint primary key)",
            cfg.project_schema
        ),
    );
    let denied = mig(
        v_denied,
        "denied_copy",
        &format!(
            "COPY \"{}\".safe_t TO PROGRAM 'sh -c \"id\"'",
            cfg.project_schema
        ),
    );

    let err = apply(&conn, &cfg, &[safe, denied], Approval::None, "actor")
        .await
        .expect_err("guard must abort the whole batch");
    assert!(
        matches!(err, ApplyError::Guard { .. }),
        "expected Guard error, got {err:?}"
    );

    assert!(
        !table_exists(&conn, &cfg.project_schema, "safe_t").await,
        "the earlier safe migration must NOT have been applied"
    );
    assert_eq!(
        journal_count(&conn, &cfg).await,
        0,
        "a denied batch journals nothing"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// H2 — statement_timeout / search_path do not leak onto the session post-apply.
// ---------------------------------------------------------------------------
#[compio::test]
async fn h2_session_settings_do_not_leak_after_apply() {
    let conn = pg().await;
    let tok = token();
    let mut cfg = cfg_for(&tok);
    cfg.pg.statement_timeout = Duration::from_millis(12_345);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let before_st = show_guc(&conn, "statement_timeout").await;
    let before_sp = show_guc(&conn, "search_path").await;

    let m = mig(
        MigrationId::generate(),
        "leak_check",
        &format!(
            "CREATE TABLE \"{}\".leakt (id bigint primary key)",
            cfg.project_schema
        ),
    );
    apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("apply");

    let after_st = show_guc(&conn, "statement_timeout").await;
    let after_sp = show_guc(&conn, "search_path").await;
    assert_eq!(
        before_st, after_st,
        "statement_timeout must be unchanged after apply (was {before_st}, now {after_st})"
    );
    assert_eq!(
        before_sp, after_sp,
        "search_path must be unchanged after apply (was {before_sp}, now {after_sp})"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// H2 — also true for the non-txn path (it SETs session-level, must restore).
// ---------------------------------------------------------------------------
#[compio::test]
async fn h2_session_settings_do_not_leak_after_non_txn_apply() {
    let conn = pg().await;
    let tok = token();
    let mut cfg = cfg_for(&tok);
    cfg.pg.statement_timeout = Duration::from_millis(23_456);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".nl (id bigint primary key, v text)",
        cfg.project_schema
    ))
    .await
    .expect("seed table");

    let before_st = show_guc(&conn, "statement_timeout").await;
    let before_sp = show_guc(&conn, "search_path").await;

    let m = mig_nontxn(
        MigrationId::generate(),
        "idx_nl_v",
        &format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_nl_v ON \"{}\".nl (v)",
            cfg.project_schema
        ),
    );
    apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect("non-txn apply");

    assert_eq!(before_st, show_guc(&conn, "statement_timeout").await);
    assert_eq!(before_sp, show_guc(&conn, "search_path").await);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// H3 — per-migration timeout: a longer override completes a pg_sleep the
// default would kill; the default still kills an un-overridden long sleep.
// ---------------------------------------------------------------------------
#[compio::test]
async fn h3_per_migration_timeout_override_lets_long_migration_complete() {
    let conn = pg().await;
    let tok = token();
    let mut cfg = cfg_for(&tok);
    // Default would kill anything over 300ms.
    cfg.pg.statement_timeout = Duration::from_millis(300);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // This migration sleeps 1s but raises its own ceiling to 5s.
    let mut slow = mig(MigrationId::generate(), "slow_ok", "SELECT pg_sleep(1)");
    slow.flags.timeout_ms = Some(5_000);
    slow.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&slow));

    let out = apply(&conn, &cfg, std::slice::from_ref(&slow), Approval::None, "actor")
        .await
        .expect("per-migration timeout override must let it complete");
    assert_eq!(out.applied.len(), 1);

    // A second migration with NO override sleeping past the default still dies.
    let fast = mig(MigrationId::generate(), "slow_killed", "SELECT pg_sleep(2)");
    let err = apply(&conn, &cfg, std::slice::from_ref(&fast), Approval::None, "actor")
        .await
        .expect_err("default timeout must still kill an un-overridden long sleep");
    assert!(
        matches!(err, ApplyError::MigrationFailed { .. }),
        "expected MigrationFailed, got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Defense-in-depth approval gate (design §1.6) — the executor enforces approval
// ITSELF, independent of the engine gate. A caller driving `apply`/`rollback`
// directly (bypassing the engine) still cannot run a destructive batch /
// rollback without explicit Approval::Approved.
// ---------------------------------------------------------------------------

/// A destructive migration (`flags.destructive = true`) with a benign,
/// re-runnable `up`/`down` so the test isolates the APPROVAL gate (not the
/// guard or a SQL failure).
fn mig_destructive(version: MigrationId, name: &str, up: &str, down: &str) -> Migration {
    let mut m = mig(version, name, up);
    m.down = Some(down.to_string());
    m.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m));
    m.flags.destructive = true;
    m.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m));
    m
}

#[compio::test]
async fn executor_apply_refuses_destructive_batch_without_approval() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let m = mig_destructive(
        MigrationId::generate(),
        "create_destructive",
        &format!(
            "CREATE TABLE \"{}\".destructive_t (id bigint primary key)",
            cfg.project_schema
        ),
        &format!("DROP TABLE \"{}\".destructive_t", cfg.project_schema),
    );

    // Approval::None → the executor's OWN gate refuses, applies NOTHING.
    let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "actor")
        .await
        .expect_err("destructive batch must be refused without approval");
    assert!(
        matches!(err, ApplyError::ApprovalRequired),
        "expected ApprovalRequired, got {err:?}"
    );
    assert!(
        !table_exists(&conn, &cfg.project_schema, "destructive_t").await,
        "a refused destructive apply must create nothing"
    );
    // The gate refuses before even bootstrapping the journal, so the journal
    // table must not exist — proof nothing was applied or recorded.
    assert!(
        !table_exists(&conn, &cfg.pg.meta_schema, "schema_migrations").await,
        "a refused destructive apply must not even bootstrap the journal"
    );

    // Approval::Approved → it applies.
    let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::Approved, "actor")
        .await
        .expect("approved destructive apply must run");
    assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
    assert!(
        table_exists(&conn, &cfg.project_schema, "destructive_t").await,
        "an approved destructive apply must run the up"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn executor_rollback_refuses_without_approval() {
    use zeroship_migrate::executor::{rollback, RollbackError, RollbackRequest, RollbackTarget};

    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // Seed one applied (reversible) migration, approving the seed apply.
    let m = mig_destructive(
        MigrationId::generate(),
        "seed_for_rollback",
        &format!(
            "CREATE TABLE \"{}\".rollback_t (id bigint primary key)",
            cfg.project_schema
        ),
        &format!("DROP TABLE \"{}\".rollback_t", cfg.project_schema),
    );
    apply(&conn, &cfg, std::slice::from_ref(&m), Approval::Approved, "actor")
        .await
        .expect("seed apply");
    assert!(table_exists(&conn, &cfg.project_schema, "rollback_t").await);

    // Approval::None → rollback refuses (every down is destructive), rolls back NOTHING.
    let err = rollback(
        &conn,
        &cfg,
        std::slice::from_ref(&m),
        RollbackRequest::new(RollbackTarget::All),
        Approval::None,
        "actor",
    )
    .await
    .expect_err("rollback must be refused without approval");
    assert!(
        matches!(err, RollbackError::ApprovalRequired),
        "expected ApprovalRequired, got {err:?}"
    );
    assert!(
        table_exists(&conn, &cfg.project_schema, "rollback_t").await,
        "a refused rollback must NOT run the down (table still present)"
    );

    drop_schemas(&conn, &cfg).await;
}

/// REGRESSION (P6): the per-app deploy schema is `"<app_id>"` where `<app_id>`
/// is a hyphenated UUID (the worker injects `Uuid::to_string()` as APP_ID;
/// plugin-db's schema = `quote_ident(app_id)`). So the derived meta schema is
/// `"<uuid>_migrations"` — a digit-leading, hyphen-bearing identifier. The
/// immutability-trigger function name still embeds the meta_schema, so it must
/// be quoted as an identifier; pre-fix `ensure_journal` interpolated such a
/// name UNQUOTED into the DDL, failing `42601 trailing junk after numeric
/// literal`. (The trigger names themselves are now short table-local constants
/// — `zs_immutable_trg` / `zs_immutable_truncate_trg` — that fit NAMEDATALEN.)
/// This drives a full apply under a UUID-shaped project/meta schema; it failed
/// before the journal quote-ident fix and passes after.
#[compio::test]
async fn ensure_journal_and_apply_under_hyphenated_uuid_schema() {
    let Some(conn) = pg_opt().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    // A real per-app id: a hyphenated UUID, exactly what the platform uses.
    let app_id = uuid::Uuid::now_v7().to_string();
    let mut cfg = ExecutorConfig::new(app_id.clone(), app_id.clone());
    cfg.pg.meta_schema = format!("{app_id}_migrations");
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    // Bootstrapping the journal under the hyphenated meta schema must NOT 42601.
    ensure_journal(&conn, &cfg)
        .await
        .expect("ensure_journal must succeed under a hyphenated UUID meta schema");

    // And a real apply lands the table + journals — the full path works.
    let m = mig(
        MigrationId::generate(),
        "create_hyphen_t",
        "CREATE TABLE hyphen_t (id int PRIMARY KEY);",
    );
    apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "deploy")
        .await
        .expect("apply under hyphenated schema");
    assert!(
        table_exists(&conn, &cfg.project_schema, "hyphen_t").await,
        "table created in the hyphenated per-app schema"
    );
    assert_eq!(
        journal::applied_count(&conn, &cfg).await.expect("applied_count"),
        1,
        "the migration is journaled in the hyphenated meta schema"
    );

    drop_schemas(&conn, &cfg).await;
}

/// Like [`pg`] but returns `None` when the DB is unreachable (for the skip
/// path of the regression test above).
async fn pg_opt() -> Option<Client> {
    match compio_postgres::connect(&dsn(), compio_postgres::NoTls).await {
        Ok((client, conn)) => {
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();
            Some(client)
        }
        Err(e) => {
            // PR9d (2) — extend the faithful-e2e hard-gate to the migrate crate.
            // `pg_opt()`'s sole caller silently skips when :5440 is down. Under
            // MIGRATE_REQUIRE_DB that must be a HARD failure (an unreachable test DB is
            // not a vacuous green pass), mirroring the control crate's `admin_conn`
            // gate. This hard-gates every `pg_opt()` caller at once.
            assert!(
                std::env::var("MIGRATE_REQUIRE_DB").is_err(),
                "MIGRATE_REQUIRE_DB is set but zeroship_migrate_test on :5440 is unreachable \
                 ({e}); the faithful-deploy security suite must NOT silently skip in CI — \
                 a missing test DB is a hard failure, not a vacuous green pass"
            );
            None
        }
    }
}
