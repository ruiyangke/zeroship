//! PR9a — the `resolve-pending --apply|--abort` CLI runner, faithful e2e on REAL
//! Postgres (`:5440`). Drives the REAL `guard::platform_runner::run_resolve_pending`
//! (not a spawned process, not a shim) against a real obligation the engine wrote.
//!
//! - `--apply` discharges the obligation: the deferred contract (drop dual-write
//!   trigger + drop old column) applies under the REAL `Approval::Approved` gate,
//!   the obligation is journaled `resolved=applied`, and the old column is gone.
//! - `--abort` discharges the obligation the OTHER way: the dual-write trigger +
//!   the shadow (`to`) column are dropped (pre-rename shape restored, the `from`
//!   column intact), journaled `resolved=aborted`.
//! - both paths are GATED on `--yes` (destructive — the human checkpoint); without
//!   it the runner refuses and applies nothing.
//! - a SQLite DSN is refused (no pending partition).
//!
//! Each test uses a unique schema/meta so it never collides.

use compio_postgres::Client;
use zeroship_migrate::drift::{ColumnSnapshot, TableSnapshot};
use zeroship_migrate::guard::platform_runner::{
    run_resolve_pending, RunConfig, RunError, RunProfile, RunReport,
};
use zeroship_migrate::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::ir_author::{IrAuthor, LiveSchema};
use zeroship_migrate::plan::{PlanStep, RenameStep};
use zeroship_migrate::{
    provision_migrator, role::deprovision_migrator, Approval, ExecutorConfig, MigrationEngine,
    PostgresBackend, SqlDialect,
};

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

/// A Confined `ExecutorConfig` over a unique schema/meta — used BOTH to seed the
/// obligation (via the engine) and, byte-identically, by `run_resolve_pending`'s
/// `build_exec_cfg` for the Confined `RunConfig` below, so the runner re-authors
/// the contract against the SAME project schema the obligation was created under.
fn exec_cfg(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

fn run_cfg(cfg: &ExecutorConfig, yes: bool) -> RunConfig {
    RunConfig {
        dir: std::env::temp_dir().join(format!("zsmig_resolve_{}", cfg.project_schema)),
        database_url: dsn(),
        engine_override: None,
        profile: RunProfile::Confined,
        project_id: cfg.project_id.clone(),
        project_schema: cfg.project_schema.clone(),
        schemas: vec![cfg.project_schema.clone()],
        extensions: Vec::new(),
        meta_schema: cfg.pg.meta_schema.clone(),
        yes,
        statement_timeout: std::time::Duration::from_secs(60),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}; \
         DROP SCHEMA IF EXISTS {1} CASCADE; CREATE SCHEMA {1};",
        cfg.project_schema, cfg.pg.meta_schema
    ))
    .await
    .expect("create schemas");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn q(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name=$2 AND column_name=$3",
            &[&schema, &table, &column],
        )
        .await
        .expect("introspect column");
    !rows.is_empty()
}

async fn trigger_count(conn: &Client, schema: &str, table: &str) -> i64 {
    let row = conn
        .query_one(
            "SELECT count(*)::bigint AS n FROM information_schema.triggers \
             WHERE trigger_schema=$1 AND event_object_table=$2",
            &[&schema, &table],
        )
        .await
        .expect("count triggers");
    row.get("n")
}

fn live_with_column(table: &str, column: &str, data_type: &str) -> LiveSchema {
    let snap = TableSnapshot {
        columns: vec![ColumnSnapshot {
            name: column.into(),
            data_type: data_type.into(),
            nullable: true,
            default: None,
            encryption_sentinel: None,
            comment_sentinel: None,
        }],
        indexes: vec![],
        constraints: vec![],
        stored_create_sql: None,
    };
    let mut live = LiveSchema::default();
    live.tables.insert(table.into());
    live.table_snapshots.insert(table.into(), snap);
    live
}

fn rename_ir(table: &str, from: &str, to: &str) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: format!("rename_{from}_to_{to}"),
        owner_app: "app_test".into(),
        ops: vec![Op::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Seed: create members(id, email)+seed, EXPAND email → email_address (leaving the
/// contract pending). Returns the `pending_version` (the E2 trigger id).
async fn seed_pending_rename(conn: &Client, cfg: &ExecutorConfig) -> String {
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(conn);

    let create = zeroship_migrate::migration::Migration {
        version: zeroship_migrate::migration::MigrationId::generate(),
        name: "create_members".into(),
        up: format!(
            "CREATE TABLE {s}.members (id bigint PRIMARY KEY, email text); \
             INSERT INTO {s}.members (id, email) VALUES (1,'ada@x.test')"
        ),
        down: None,
        checksum: zeroship_migrate::migration::Checksum::of(
            &zeroship_migrate::migration::ChecksumInput {
                up: "create_members",
                down: None,
                flags: &zeroship_migrate::migration::MigrationFlags::default(),
                owner_app: "app_test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags: zeroship_migrate::migration::MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
    };
    engine
        .apply_plan(
            &[PlanStep::Ddl(create)],
            Approval::None,
            &be,
            cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create members");

    let live = live_with_column("members", "email", "text");
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let steps = author
        .lower_steps(&rename_ir("members", "email", "email_address"), &live)
        .expect("rename lowers");
    let pending_version = match &steps[0] {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
            ec.trigger_version.as_str().to_string()
        }
        other => panic!("expected PgExpandContract, got {other:?}"),
    };
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("expand applies, contract pending");
    pending_version
}

async fn resolved_count(conn: &Client, cfg: &ExecutorConfig, resolution: &str, v: &str) -> i64 {
    let row = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM {}.schema_pending_contracts \
                 WHERE state='resolved' AND resolution=$1 AND pending_version=$2",
                cfg.pg.meta_schema
            ),
            &[&resolution, &v],
        )
        .await
        .expect("count resolved rows");
    row.get("n")
}

// ---------------------------------------------------------------------------

/// `resolve-pending --apply` discharges the obligation: the contract applies under
/// the real approval gate (trigger + old column gone), and a `resolved=applied`
/// row is journaled (append-only).
#[compio::test]
async fn resolve_pending_apply_discharges_and_applies_the_contract() {
    let conn = pg().await;
    let cfg = exec_cfg(&token());
    setup(&conn, &cfg).await;
    let pending_version = seed_pending_rename(&conn, &cfg).await;

    // Pre: the dual-write trigger + the old `email` column both exist (EXPAND ran).
    assert!(trigger_count(&conn, &cfg.project_schema, "members").await >= 1);
    assert!(column_exists(&conn, &cfg.project_schema, "members", "email").await);

    // --apply: ack is IGNORED (apply preserves data) — pass false to prove apply does
    // not require the abort-only acknowledgement.
    let report = run_resolve_pending(&run_cfg(&cfg, true), &pending_version, true, false, false)
        .await
        .expect("resolve-pending --apply succeeds (no shadow-data-loss ack required)");
    match report {
        RunReport::ResolvePending(o) => {
            assert_eq!(o.pending_version, pending_version);
            assert_eq!(o.table, "members");
            assert_eq!(o.resolution, zeroship_migrate::Resolution::Applied);
            assert!(
                o.data_loss_warning.is_none(),
                "--apply preserves data — no data-loss warning"
            );
        }
        other => panic!("expected ResolvePending, got {other:?}"),
    }

    // The contract applied: trigger gone + old `email` column gone.
    assert_eq!(
        trigger_count(&conn, &cfg.project_schema, "members").await,
        0,
        "the dual-write trigger is dropped by the contract"
    );
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the old column is dropped by the contract"
    );
    assert!(column_exists(&conn, &cfg.project_schema, "members", "email_address").await);

    // Journaled resolved=applied (append-only); no longer outstanding.
    assert_eq!(resolved_count(&conn, &cfg, "applied", &pending_version).await, 1);
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert!(outstanding.is_empty(), "the obligation is discharged");

    teardown(&conn, &cfg).await;
}

/// `resolve-pending --abort` discharges the obligation the OTHER way: the
/// dual-write trigger + the SHADOW (`email_address`) column are dropped (pre-rename
/// shape — `email` intact), journaled `resolved=aborted`.
#[compio::test]
async fn resolve_pending_abort_drops_trigger_and_shadow_column() {
    let conn = pg().await;
    let cfg = exec_cfg(&token());
    setup(&conn, &cfg).await;
    let pending_version = seed_pending_rename(&conn, &cfg).await;

    // --abort: requires the DISTINCT --acknowledge-shadow-data-loss (ack=true).
    let report = run_resolve_pending(&run_cfg(&cfg, true), &pending_version, false, true, true)
        .await
        .expect("resolve-pending --abort succeeds with the shadow-data-loss ack");
    match report {
        RunReport::ResolvePending(o) => {
            assert_eq!(o.resolution, zeroship_migrate::Resolution::Aborted);
            let warning = o
                .data_loss_warning
                .expect("--abort outcome MUST carry the data-loss warning");
            assert!(
                warning.contains("DATA DISCARDED") && warning.contains("email_address"),
                "the abort warning names the discarded shadow column: {warning}"
            );
        }
        other => panic!("expected ResolvePending, got {other:?}"),
    }

    // Abort restores pre-rename shape: trigger gone, shadow `email_address` gone,
    // original `email` INTACT (C2 — drop old column — never ran).
    assert_eq!(trigger_count(&conn, &cfg.project_schema, "members").await, 0);
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "email_address").await,
        "the shadow column is dropped by --abort"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the original column is INTACT after --abort"
    );
    assert_eq!(resolved_count(&conn, &cfg, "aborted", &pending_version).await, 1);
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert!(outstanding.is_empty(), "the obligation is discharged by abort");

    teardown(&conn, &cfg).await;
}

/// Without `--yes`, resolve-pending REFUSES (both paths are destructive — the human
/// checkpoint) and applies nothing — the obligation stays outstanding.
#[compio::test]
async fn resolve_pending_without_yes_is_refused() {
    let conn = pg().await;
    let cfg = exec_cfg(&token());
    setup(&conn, &cfg).await;
    let pending_version = seed_pending_rename(&conn, &cfg).await;

    let err = run_resolve_pending(&run_cfg(&cfg, false), &pending_version, true, false, false)
        .await
        .expect_err("resolve-pending without --yes must refuse");
    assert!(matches!(err, RunError::ResolvePending(_)), "expected a ResolvePending refusal, got {err:?}");

    // Nothing applied: the obligation is still outstanding, the trigger still live.
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert_eq!(outstanding.len(), 1, "the obligation is untouched without --yes");
    assert!(trigger_count(&conn, &cfg.project_schema, "members").await >= 1);

    teardown(&conn, &cfg).await;
}

/// A missing/unknown version is an honest refusal (no obligation outstanding for it),
/// and requesting BOTH or NEITHER flag is refused.
#[compio::test]
async fn resolve_pending_bad_version_or_flags_is_refused() {
    let conn = pg().await;
    let cfg = exec_cfg(&token());
    setup(&conn, &cfg).await;
    let pending_version = seed_pending_rename(&conn, &cfg).await;

    // Both flags.
    let err = run_resolve_pending(&run_cfg(&cfg, true), &pending_version, true, true, false)
        .await
        .expect_err("both --apply and --abort must refuse");
    assert!(matches!(err, RunError::ResolvePending(_)));

    // Neither flag.
    let err = run_resolve_pending(&run_cfg(&cfg, true), &pending_version, false, false, false)
        .await
        .expect_err("neither flag must refuse");
    assert!(matches!(err, RunError::ResolvePending(_)));

    // Unknown version (not outstanding).
    let err = run_resolve_pending(&run_cfg(&cfg, true), "mig_does_not_exist", true, false, false)
        .await
        .expect_err("unknown version must refuse");
    assert!(matches!(err, RunError::ResolvePending(_)));

    teardown(&conn, &cfg).await;
}

/// **PR9b L3** — `--abort` WITHOUT the distinct `--acknowledge-shadow-data-loss` is
/// REFUSED even WITH `--yes`. The data-discarding abort requires a SEPARATE
/// acknowledgement that the shared `--yes` (which `--apply` uses) does NOT satisfy —
/// so a blanket `--yes` cannot silently authorize discarding shadow-column data. The
/// obligation is left untouched (still outstanding, trigger + shadow column live).
///
/// RED PRE-FIX: `--abort` gated only on `--yes`, so `(--yes, --abort)` succeeded and
/// dropped the shadow column. POST-FIX it is refused without the ack.
#[compio::test]
async fn abort_without_shadow_ack_is_refused_even_with_yes() {
    let conn = pg().await;
    let cfg = exec_cfg(&token());
    setup(&conn, &cfg).await;
    let pending_version = seed_pending_rename(&conn, &cfg).await;

    // --yes is present, but the distinct shadow-data-loss ack is NOT.
    let err = run_resolve_pending(&run_cfg(&cfg, true), &pending_version, false, true, false)
        .await
        .expect_err("--abort without --acknowledge-shadow-data-loss must refuse even with --yes");
    match err {
        RunError::ResolvePending(msg) => assert!(
            msg.contains("acknowledge-shadow-data-loss") && msg.contains("SEPARATE from --yes"),
            "the refusal must name the distinct ack and that it is separate from --yes: {msg}"
        ),
        other => panic!("expected a ResolvePending refusal, got {other:?}"),
    }

    // FAIL CLOSED: the obligation is untouched — still outstanding, shadow column +
    // trigger still live (the abort applied nothing).
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert_eq!(outstanding.len(), 1, "the obligation is untouched without the abort ack");
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email_address").await,
        "the shadow column is INTACT — --abort applied nothing without the ack"
    );
    assert!(trigger_count(&conn, &cfg.project_schema, "members").await >= 1);

    teardown(&conn, &cfg).await;
}
