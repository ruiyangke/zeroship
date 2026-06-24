//! PR9a — §2.0.3/§2.0.4 cross-deploy pending-contract INTERLOCK, faithful e2e on
//! REAL Postgres (`:5440`). No shims, no PG-gated skips.
//!
//! These tests pin the engine-level, author-agnostic, fail-closed guarantees the
//! interlock exists to provide:
//!
//! - (persist) a completed `PgExpandContract` EXPAND writes a DURABLE obligation
//!   that a fresh read (a later deploy / restarted process) reads back as
//!   outstanding (§2.0.3 item 1);
//! - (b) a deploy whose op list TOUCHES the pending table is REFUSED with the
//!   structured `TABLE_HAS_PENDING_CONTRACT` payload carrying an executable
//!   `apply_action` (§2.0.3 item 2);
//! - (c) a SECOND `renameColumn` on the same column while a contract is pending is
//!   refused;
//! - (d) an ORPHANED pending contract fails closed, is surfaced by `status` as a
//!   distinct orphan state, and emits `ORPHANED_PENDING_CONTRACT` (§2.0.3 item 3);
//! - (e) re-running deploy N after EXPAND is a no-op — same obligation re-surfaced,
//!   nothing re-applied, no duplicate `pending` row (§2.0.3 item 4);
//! - (apply/discharge) the deploy-N+1 contract-apply discharges the obligation
//!   (journaled `resolved`, append-only) and the table is clear again;
//! - (§2.0.4) plan B `depends_on` plan A whose contract is pending is BLOCKED,
//!   surfaced as a retained `blocked-awaiting-approval` state with
//!   `DEPENDENCY_PENDING_CONTRACT`, NOT failed.
//!
//! Every assertion is against the real applied schema, the real journal, and the
//! real obligation table.

use compio_postgres::Client;
use zeroship_migrate::drift::{ColumnSnapshot, TableSnapshot};
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

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
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

fn rename_ir(table: &str, from: &str, to: &str, ty: ColType) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: format!("rename_{from}_to_{to}"),
        owner_app: "app_test".into(),
        ops: vec![Op::RenameColumn { table: table.into(), from: from.into(), to: to.into(), ty }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Lower a one-op rename IR on the PG leg and unwrap its `PgExpandContract`.
fn lower_pg_rename(
    cfg: &ExecutorConfig,
    table: &str,
    from: &str,
    to: &str,
    ty: ColType,
    live: &LiveSchema,
) -> zeroship_migrate::expand_contract::ExpandContractPlan {
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = rename_ir(table, from, to, ty);
    let steps = author.lower_steps(&ir, live).expect("PG rename lowers");
    match steps.into_iter().next().unwrap() {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => ec,
        other => panic!("PG must lower to a PgExpandContract, got {other:?}"),
    }
}

/// Build the `members(id, email)` table + seed, via a plain Ddl plan.
async fn create_members(engine: &MigrationEngine, be: &PostgresBackend<'_>, cfg: &ExecutorConfig) {
    let s = q(&cfg.project_schema);
    let m = zeroship_migrate::migration::Migration {
        version: zeroship_migrate::migration::MigrationId::generate(),
        name: "create_members".into(),
        up: format!(
            "CREATE TABLE {s}.members (id bigint PRIMARY KEY, email text); \
             INSERT INTO {s}.members (id, email) VALUES (1,'ada@x.test'),(2,'gr@x.test')"
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
            &[PlanStep::Ddl(m)],
            Approval::None,
            be,
            cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create members");
}

/// Apply the EXPAND of `email → email_address` on `members`, leaving the contract
/// pending. Returns the expand-contract plan (for its contract versions/intent).
async fn expand_email_rename(
    engine: &MigrationEngine,
    be: &PostgresBackend<'_>,
    cfg: &ExecutorConfig,
) -> zeroship_migrate::expand_contract::ExpandContractPlan {
    let live = live_with_column("members", "email", "text");
    let ec = lower_pg_rename(cfg, "members", "email", "email_address", ColType::Text, &live);
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec.clone()))];
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            be,
            cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("expand applies");
    ec
}

/// A plain Ddl `addColumn` step targeting `table` (with the IR-derived touched-set
/// so the engine interlock sees the table as touched even for plain DDL).
fn addcolumn_step(cfg: &ExecutorConfig, table: &str, column: &str) -> (PlanStep, Vec<String>) {
    let s = q(&cfg.project_schema);
    let m = zeroship_migrate::migration::Migration {
        version: zeroship_migrate::migration::MigrationId::generate(),
        name: format!("add_{column}"),
        up: format!("ALTER TABLE {s}.{table} ADD COLUMN {column} text"),
        down: None,
        checksum: zeroship_migrate::migration::Checksum::of(
            &zeroship_migrate::migration::ChecksumInput {
                up: "addcol",
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
    (PlanStep::Ddl(m), vec![table.to_string()])
}

// ----------------------------------------------------------------------------

/// PERSIST + READ-BACK (§2.0.3 item 1): a completed EXPAND writes a durable
/// obligation a FRESH read returns as outstanding — keyed by (table,
/// pending_version), carrying the C1/C2 contract versions. The obligation is the
/// SOURCE OF TRUTH the interlock reads, not the transient `pending_contract`.
#[compio::test]
async fn expand_writes_durable_outstanding_obligation_read_back_fresh() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // A FRESH read (a separate call — the later-deploy / restart vector) sees ONE
    // outstanding obligation for `members`, keyed by the E2 trigger version.
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert_eq!(outstanding.len(), 1, "exactly one obligation outstanding");
    let pc = &outstanding[0];
    assert_eq!(pc.table, "members");
    assert_eq!(pc.from_col, "email");
    assert_eq!(pc.to_col, "email_address");
    assert_eq!(
        pc.pending_version,
        ec.trigger_version.as_str(),
        "the obligation key is the E2 trigger version"
    );
    let contract_ids: Vec<String> =
        ec.contract.iter().map(|m| m.version.as_str().to_string()).collect();
    assert_eq!(pc.contract_versions, contract_ids, "C1/C2 ids recorded");

    teardown(&conn, &cfg).await;
}

/// (b) REFUSAL with the structured payload: a deploy whose op list touches the
/// pending table is FAIL-CLOSED refused with `TABLE_HAS_PENDING_CONTRACT`, and the
/// payload carries an EXECUTABLE `apply_action` (`migrate resolve-pending --apply
/// <version>`). Nothing applies — the touching column is NOT added.
#[compio::test]
async fn deploy_touching_pending_table_is_refused_with_executable_payload() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // A routine deploy that ADD COLUMNs on `members` — touches the pending table.
    let (step, touched) = addcolumn_step(&cfg, "members", "nickname");
    let err = engine
        .apply_plan_with_touched(
            &[step],
            &touched,
            Approval::None,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect_err("a deploy touching the pending table must be refused");

    match err {
        zeroship_migrate::DeclarativeApplyError::Plain(
            zeroship_migrate::EngineError::PendingContract(payload),
        ) => {
            assert_eq!(payload.code, zeroship_migrate::CODE_TABLE_HAS_PENDING_CONTRACT);
            assert_eq!(payload.table, "members");
            assert_eq!(payload.pending_version, ec.trigger_version.as_str());
            assert_eq!(payload.apply_action.command, "migrate resolve-pending --apply");
            assert_eq!(payload.apply_action.version, ec.trigger_version.as_str());
        }
        other => panic!("expected a PendingContract refusal, got {other:?}"),
    }

    // FAIL CLOSED: the touching column was NOT added — nothing applied.
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "nickname").await,
        "the refused deploy applied NOTHING"
    );

    teardown(&conn, &cfg).await;
}

/// (c) a SECOND `renameColumn` on the same column while the prior contract is
/// pending is refused — its EXPAND steps target the pending table.
#[compio::test]
async fn second_rename_on_pending_table_is_refused() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let _ec = expand_email_rename(&engine, &be, &cfg).await;

    // A second online rename on `members` (email_address → contact) while the
    // first contract is pending. Its EXPAND step names `members` (the rename
    // intent's table), so the interlock refuses even with an empty touched-set.
    let live2 = live_with_column("members", "email_address", "text");
    let ec2 = lower_pg_rename(&cfg, "members", "email_address", "contact", ColType::Text, &live2);
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec2))];
    let err = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect_err("a second rename on the pending table must be refused");
    assert!(
        matches!(
            err,
            zeroship_migrate::DeclarativeApplyError::Plain(
                zeroship_migrate::EngineError::PendingContract(_)
            )
        ),
        "expected a PendingContract refusal, got {err:?}"
    );

    teardown(&conn, &cfg).await;
}

/// (e) IDEMPOTENT re-run of deploy N (EXPAND already journaled): re-applying the
/// SAME expand plan re-acquires the lock, net-applied-skips E1..E3+backfill,
/// re-surfaces the SAME obligation, and is a no-op — NO duplicate `pending` row.
#[compio::test]
async fn re_running_expand_is_a_noop_no_duplicate_obligation() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // Re-run the SAME expand (deploy N retried). It must net-applied-skip and NOT
    // append a second `pending` row.
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec.clone()))];
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("re-run expand is a no-op");

    // Still exactly one OUTSTANDING obligation (net state), and exactly one raw
    // `pending` row in the append-only log (no duplicate).
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert_eq!(outstanding.len(), 1, "still one obligation outstanding after re-run");

    let row = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM {}.schema_pending_contracts \
                 WHERE state='pending' AND pending_version=$1",
                cfg.pg.meta_schema
            ),
            &[&ec.trigger_version.as_str()],
        )
        .await
        .expect("count pending rows");
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "re-run must NOT append a duplicate `pending` row");

    teardown(&conn, &cfg).await;
}

/// APPLY/DISCHARGE: the deploy-N+1 contract-apply (C1/C2 as Ddl steps) discharges
/// the obligation — a `resolved` row is APPENDED (append-only), the table is clear
/// again (a later op on it is no longer refused), and the old column is gone.
#[compio::test]
async fn contract_apply_discharges_obligation_and_clears_the_table() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // Deploy N+1: apply the contract (C1 drop trigger + C2 drop old `email`).
    let contract_steps: Vec<PlanStep> =
        ec.contract.iter().cloned().map(PlanStep::Ddl).collect();
    engine
        .apply_plan(
            &contract_steps,
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("contract applies and discharges");

    // The obligation is discharged — no longer outstanding.
    let outstanding = zeroship_migrate::outstanding_pending_contracts(&conn, &cfg)
        .await
        .expect("read outstanding");
    assert!(outstanding.is_empty(), "the obligation is discharged (no longer outstanding)");

    // History is append-only: a `resolved='applied'` row exists.
    let row = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM {}.schema_pending_contracts \
                 WHERE state='resolved' AND resolution='applied' AND pending_version=$1",
                cfg.pg.meta_schema
            ),
            &[&ec.trigger_version.as_str()],
        )
        .await
        .expect("count resolved rows");
    let n: i64 = row.get("n");
    assert_eq!(n, 1, "exactly one `resolved=applied` discharge row appended");

    // The old `email` column is gone; the table is clear — a later op on it is
    // no longer refused.
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the old column is dropped by the contract"
    );
    let (step, touched) = addcolumn_step(&cfg, "members", "nickname");
    engine
        .apply_plan_with_touched(
            &[step],
            &touched,
            Approval::None,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("a later op on the cleared table is no longer refused");
    assert!(column_exists(&conn, &cfg.project_schema, "members", "nickname").await);

    teardown(&conn, &cfg).await;
}

/// (d) ORPHAN (§2.0.3 item 3): when a later bundle no longer carries the rename
/// whose contract is pending, `status` surfaces the obligation as a DISTINCT
/// orphan state (`orphaned: true`), and the structured `ORPHANED_PENDING_CONTRACT`
/// payload carries an executable `abort_action`. A non-orphan (the rename still in
/// the set) is `orphaned: false`.
#[compio::test]
async fn orphaned_pending_contract_is_surfaced_by_status_as_distinct_state() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // status() against a migration set that DOES NOT carry the rename's expand id
    // ⇒ the obligation is ORPHANED (the rename was removed after its EXPAND).
    let st = zeroship_migrate::status(&conn, &cfg, &[])
        .await
        .expect("status reads the obligation");
    assert_eq!(st.pending_contracts.len(), 1, "the obligation is surfaced by status");
    let pcs = &st.pending_contracts[0];
    assert_eq!(pcs.table, "members");
    assert_eq!(pcs.pending_version, ec.trigger_version.as_str());
    assert!(pcs.orphaned, "the rename is absent from the set ⇒ ORPHANED");

    // The structured ORPHANED payload carries an executable abort_action.
    let payload =
        zeroship_migrate::OrphanedPendingContract::new(pcs.table.clone(), pcs.pending_version.clone());
    assert_eq!(payload.code, zeroship_migrate::CODE_ORPHANED_PENDING_CONTRACT);
    assert_eq!(payload.abort_action.command, "migrate resolve-pending --abort");
    assert_eq!(payload.abort_action.version, ec.trigger_version.as_str());

    // And when the rename's expand id IS present in the supplied set, the same
    // obligation is NOT orphaned (a routine outstanding contract).
    let still_present = zeroship_migrate::migration::Migration {
        version: ec.trigger_version.clone(),
        name: "expand".into(),
        up: "SELECT 1".into(),
        down: None,
        checksum: zeroship_migrate::migration::Checksum::of(
            &zeroship_migrate::migration::ChecksumInput {
                up: "x",
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
    let st2 = zeroship_migrate::status(&conn, &cfg, std::slice::from_ref(&still_present))
        .await
        .expect("status with the rename present");
    assert_eq!(st2.pending_contracts.len(), 1);
    assert!(
        !st2.pending_contracts[0].orphaned,
        "the rename is present in the set ⇒ NOT orphaned"
    );

    teardown(&conn, &cfg).await;
}

/// §2.0.4 — a plan B `depends_on` plan A whose online-rename contract is pending
/// is BLOCKED: `status` lists B in `blocked` (naming A's pending version), a
/// DISTINCT retained `blocked-awaiting-approval` state — NOT a failure. After A's
/// contract applies, the same status read no longer blocks B.
#[compio::test]
async fn dependent_plan_blocks_on_a_pending_contract_dependency() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    create_members(&engine, &be, &cfg).await;
    let ec = expand_email_rename(&engine, &be, &cfg).await;

    // Plan B declares depends_on: [A's expand id] (A = the pending online rename).
    let b = zeroship_migrate::migration::Migration {
        version: zeroship_migrate::migration::MigrationId::generate(),
        name: "plan_b".into(),
        up: "SELECT 1".into(),
        down: None,
        checksum: zeroship_migrate::migration::Checksum::of(
            &zeroship_migrate::migration::ChecksumInput {
                up: "b",
                down: None,
                flags: &zeroship_migrate::migration::MigrationFlags::default(),
                owner_app: "app_test",
                depends_on: std::slice::from_ref(&ec.trigger_version),
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags: zeroship_migrate::migration::MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: vec![ec.trigger_version.clone()],
        supersedes: vec![],
        preconditions: vec![],
    };

    let st = zeroship_migrate::status(&conn, &cfg, std::slice::from_ref(&b))
        .await
        .expect("status with the dependent plan");
    assert_eq!(st.blocked.len(), 1, "B is recorded as blocked, NOT failed");
    let blk = &st.blocked[0];
    assert_eq!(blk.blocked.as_str(), b.version.as_str());
    assert_eq!(blk.dependency.as_str(), ec.trigger_version.as_str());
    assert_eq!(blk.pending_version, ec.trigger_version.as_str());

    // The structured DEPENDENCY_PENDING_CONTRACT payload names B, A, and the
    // pending version.
    let payload = zeroship_migrate::DependencyPendingContract::new(
        blk.blocked.as_str(),
        blk.dependency.as_str(),
        blk.pending_version.clone(),
    );
    assert_eq!(payload.code, zeroship_migrate::CODE_DEPENDENCY_PENDING_CONTRACT);

    // After A's contract applies (discharging the obligation), B no longer blocks.
    let contract_steps: Vec<PlanStep> =
        ec.contract.iter().cloned().map(PlanStep::Ddl).collect();
    engine
        .apply_plan(
            &contract_steps,
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("A's contract applies");
    let st2 = zeroship_migrate::status(&conn, &cfg, std::slice::from_ref(&b))
        .await
        .expect("status after A's contract");
    assert!(st2.blocked.is_empty(), "after A's contract, B no longer blocks (roll-forward)");

    teardown(&conn, &cfg).await;
}

/// A deploy that touches an UNRELATED table while a pending contract is
/// outstanding PROCEEDS (the interlock is per-table; an unrelated table is never
/// gated — roll-forward, §2.0.4).
#[compio::test]
async fn deploy_touching_unrelated_table_proceeds() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();
    let be = PostgresBackend::new(&conn);

    // members + a SEPARATE `widgets` table.
    create_members(&engine, &be, &cfg).await;
    conn.batch_execute(&format!("CREATE TABLE {s}.widgets (id bigint PRIMARY KEY)"))
        .await
        .expect("create widgets");
    let _ec = expand_email_rename(&engine, &be, &cfg).await;

    // An addColumn on `widgets` (unrelated to the pending `members` rename) proceeds.
    let (step, touched) = addcolumn_step(&cfg, "widgets", "label");
    engine
        .apply_plan_with_touched(
            &[step],
            &touched,
            Approval::None,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("an unrelated-table deploy proceeds despite the pending contract");
    assert!(column_exists(&conn, &cfg.project_schema, "widgets", "label").await);

    teardown(&conn, &cfg).await;
}
