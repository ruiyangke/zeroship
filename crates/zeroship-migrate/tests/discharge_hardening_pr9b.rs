//! PR9b L1 — DISCHARGE HARDENING (re-author-compare), faithful e2e on REAL Postgres
//! (`:5440`). No shims, no PG-gated skips.
//!
//! Pins the security guarantee that the contract-apply auto-discharge keys on the
//! discharging Ddl steps SEMANTICALLY MATCHING the re-authored C1/C2 — NOT a
//! version-id match alone. A FORGED plan carrying the obligation's recorded
//! `contract_versions` but INNOCUOUS `up` SQL (a `SELECT 1` / a harmless `COMMENT
//! ON`) must NOT discharge the obligation: the dual-write trigger + the `from` column
//! stay live, the obligation stays outstanding, and a subsequent op on the table is
//! still refused with `TABLE_HAS_PENDING_CONTRACT`.
//!
//! RED PRE-FIX: the old recognizer keyed PURELY on the Ddl step version covering the
//! recorded `contract_versions`, so the forged innocuous plan would have discharged
//! the obligation + un-gated the table while leaving a live trigger + un-dropped
//! column. POST-FIX it is refused.

use compio_postgres::Client;
use zeroship_migrate::{ColumnSnapshot, TableSnapshot};
use zeroship_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::render::lower::{IrAuthor, LiveSchema};
use zeroship_migrate::{PlanStep, RenameStep};
use zeroship_migrate::{
    provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig, ExpandContractPlan,
    MigrationBackend, MigrationEngine, PostgresBackend, SqlDialect,
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

async fn trigger_count(conn: &Client, schema: &str, table: &str) -> i64 {
    let row = conn
        .query_one(
            "SELECT count(*) FROM information_schema.triggers \
             WHERE trigger_schema=$1 AND event_object_table=$2",
            &[&schema, &table],
        )
        .await
        .expect("count triggers");
    row.get(0)
}

fn live_with_column(table: &str, column: &str, data_type: &str) -> LiveSchema {
    let snap = TableSnapshot {
        columns: vec![ColumnSnapshot {
            name: column.into(),
            data_type: data_type.into(),
            nullable: true,
            default: None,
            ddl_type_override: None,
            inline_checks: Vec::new(),
            generated: None,
            identity: None,
            case_sensitive: None,
            encryption_sentinel: None,
            comment_sentinel: None,
            comment: None,
        }],
        indexes: vec![],
        constraints: vec![],
        runtime_options: Default::default(),

    partition_by: None,

    comment: None,
        stored_create_sql: None,
    };
    let mut live = LiveSchema::default();
    live.tables.insert(table.into());
    live.table_snapshots.insert(table.into(), snap);
    live
}

fn lower_pg_rename(
    cfg: &ExecutorConfig,
    table: &str,
    from: &str,
    to: &str,
    live: &LiveSchema,
) -> ExpandContractPlan {
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = MigrationIr {
        ir_version: 1,
        name: format!("rename_{from}_{to}"),
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
    };
    let steps = author.lower_steps(&ir, live).expect("PG rename lowers");
    match steps.into_iter().next().unwrap() {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => ec,
        other => panic!("PG must lower to a PgExpandContract, got {other:?}"),
    }
}

/// A plain Ddl step with the GIVEN version but the GIVEN (innocuous) `up` SQL.
fn forged_ddl(version: &zeroship_migrate::model::migration::MigrationId, up: &str) -> PlanStep {
    PlanStep::Ddl(zeroship_migrate::model::migration::Migration {
        version: version.clone(),
        name: "forged_contract_step".into(),
        up: up.to_string(),
        down: None,
        checksum: zeroship_migrate::model::migration::Checksum::of(
            &zeroship_migrate::model::migration::ChecksumInput {
                up,
                down: None,
                flags: &zeroship_migrate::model::migration::MigrationFlags::default(),
                owner_app: "app_test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags: zeroship_migrate::model::migration::MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        existence_guard: None,
    })
}

// A forged "contract-apply" plan carrying the obligation's recorded contract
// version-ids but INNOCUOUS up SQL must NOT discharge the obligation.
#[compio::test]
async fn forged_contract_versionids_with_innocuous_sql_do_not_discharge() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    setup(&conn, &cfg).await;
    let be = PostgresBackend::new(&conn);
    let engine = MigrationEngine::new();
    let s = q(&cfg.project_schema);

    // Create members(id, email) + seed.
    let create = zeroship_migrate::model::migration::Migration {
        version: zeroship_migrate::model::migration::MigrationId::generate(),
        name: "create_members".into(),
        up: format!(
            "CREATE TABLE {s}.members (id bigint PRIMARY KEY, email text); \
             INSERT INTO {s}.members (id, email) VALUES (1,'a@x.test')"
        ),
        down: None,
        checksum: zeroship_migrate::model::migration::Checksum::of(
            &zeroship_migrate::model::migration::ChecksumInput {
                up: "create_members",
                down: None,
                flags: &zeroship_migrate::model::migration::MigrationFlags::default(),
                owner_app: "app_test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags: zeroship_migrate::model::migration::MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        existence_guard: None,
    };
    engine
        .apply_plan(
            &[PlanStep::Ddl(create)],
            Approval::None,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("create members");

    // EXPAND email → email_address — opens the durable pending contract.
    let live = live_with_column("members", "email", "text");
    let ec = lower_pg_rename(&cfg, "members", "email", "email_address", &live);
    engine
        .apply_plan(
            &[PlanStep::OnlineRename(RenameStep::PgExpandContract(ec.clone()))],
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("expand applies + opens obligation");

    // Sanity: the obligation is outstanding, the dual-write trigger + `email` are live.
    let pending_contracts = be
        .pending_contracts()
        .expect("Postgres pending-contract capability");
    let outstanding = pending_contracts
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read outstanding");
    assert_eq!(outstanding.len(), 1, "exactly one obligation outstanding after EXPAND");
    let triggers_before = trigger_count(&conn, &cfg.project_schema, "members").await;
    assert!(triggers_before >= 1, "the dual-write trigger must be live after EXPAND");
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the `from` column must still be live after EXPAND"
    );

    // FORGE a contract-apply: two plain Ddl steps carrying the obligation's recorded
    // C1/C2 version-ids but INNOCUOUS up SQL (a harmless COMMENT ON / SELECT can't run
    // as a migration `up` cleanly, so use harmless idempotent COMMENTs on the schema).
    let c1_v = ec.contract[0].version.clone();
    let c2_v = ec.contract[1].version.clone();
    let innocuous1 = format!("COMMENT ON SCHEMA {s} IS 'forged-step-1'");
    let innocuous2 = format!("COMMENT ON SCHEMA {s} IS 'forged-step-2'");
    let forged = vec![
        forged_ddl(&c1_v, &innocuous1),
        forged_ddl(&c2_v, &innocuous2),
    ];

    // Apply the forged plan. The touched-set names `members` (its own steps carry no
    // structured table, so pass it explicitly as the IR path would). Pre-fix this
    // would be RECOGNIZED as a contract-apply (version-ids cover the recorded set) and
    // DISCHARGE the obligation. Post-fix the re-author-compare REJECTS it: the
    // innocuous up SQL does not match the re-authored C1/C2, so it is NOT a
    // contract-apply — and because the touched-set hits the still-outstanding
    // obligation's table, the deploy is REFUSED with TABLE_HAS_PENDING_CONTRACT.
    let result = engine
        .apply_plan_with_touched(
            &forged,
            &["members".into()],
            Approval::Approved,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await;
    match result {
        Err(zeroship_migrate::DeclarativeApplyError::Plain(
            zeroship_migrate::EngineError::PendingContract(payload),
        )) => {
            assert_eq!(payload.table, "members", "the refusal names the pending table");
        }
        other => panic!(
            "a forged version-id-only contract-apply with innocuous SQL must be REFUSED \
             (TABLE_HAS_PENDING_CONTRACT), not silently discharge; got {other:?}"
        ),
    }

    // FAIL CLOSED: the obligation is STILL outstanding, the trigger + `email` still live
    // (the forged plan applied nothing and discharged nothing).
    let still = pending_contracts
        .outstanding_pending_contracts(&cfg)
        .await
        .expect("read outstanding after forged attempt");
    assert_eq!(
        still.len(),
        1,
        "the forged plan must NOT have discharged the obligation (it is still outstanding)"
    );
    assert_eq!(
        trigger_count(&conn, &cfg.project_schema, "members").await,
        triggers_before,
        "the dual-write trigger must still be live (forged C1 did NOT drop it)"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the `from` column must still be live (forged C2 did NOT drop it)"
    );

    // And a routine op on the table is STILL refused — the table stays gated.
    let touch_ir = MigrationIr {
        ir_version: 1,
        name: "add_nick".into(),
        owner_app: "app_test".into(),
        ops: vec![Op::AddColumn {
            table: "members".into(),
            column: "nick".into(),
            ty: ColType::Text,
            nullable: Some(true),
            default: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let touch_author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let touch_steps = touch_author.lower_steps(&touch_ir, &live).expect("addColumn lowers");
    let touch_result = engine
        .apply_plan_with_touched(
            &touch_steps,
            &["members".into()],
            Approval::None,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await;
    assert!(
        matches!(
            touch_result,
            Err(zeroship_migrate::DeclarativeApplyError::Plain(
                zeroship_migrate::EngineError::PendingContract(_)
            ))
        ),
        "a subsequent op on the still-gated table must be refused, got {touch_result:?}"
    );

    teardown(&conn, &cfg).await;
}
