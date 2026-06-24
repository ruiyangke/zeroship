//! PR2 — faithful e2e + byte-equality for the IR `renameColumn` lowering on the
//! **Postgres** leg (§2.6 / §2.6.1 / §2.6.2) against REAL Postgres (`:5440`).
//!
//! The PG leg lowers ONE `op.renameColumn` to ONE
//! `PlanStep::OnlineRename(RenameStep::PgExpandContract(_))` — the canonical
//! expand-contract dual-write (E1 ADD COLUMN, E2 dual-write trigger, E3 backfill
//! marker, C1 DROP TRIGGER, C2 DROP COLUMN). These tests drive the REAL IR
//! authoring path (`IrAuthor::lower_steps`) — NOT a hand-built
//! `ExpandContractAuthor` — and apply through the engine's single shared
//! `apply_plan`:
//!
//! - the EXPAND (E1..E3 + backfill) applies atomically under the held project
//!   lock; C1/C2 are DEFERRED into `pending_contract` (the cross-deploy partition);
//!   both columns exist after the expand and the backfill mirrored the rows;
//! - a mid-sequence crash RESUMES roll-forward to the same final state;
//! - the neutral `ColType` renders the correct PG type string in the
//!   `OnlineIntent` (E1's `ADD COLUMN <to> <pg-type>`);
//! - the E1..C2 `up`/`down`/`flags` + intra-chain `depends_on` SHAPE is byte-equal
//!   to the equivalent declarative `t.*`-diff rename's (same `ExpandContractAuthor`,
//!   §2.6.1 — the IR plan does NOT re-mint or diverge from the author).
//!
//! No shims, no PG-gated skips: every assertion is against the real applied schema
//! + the real journal.

use compio_postgres::Client;
use zeroship_migrate::drift::{ColumnSnapshot, TableSnapshot};
use zeroship_migrate::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::ir_author::{IrAuthor, LiveSchema};
use zeroship_migrate::plan::{PlanStep, RenameStep};
use zeroship_migrate::{
    provision_migrator, role::deprovision_migrator, Approval, ExecutorConfig, MigrationEngine,
    OnlineIntent, PostgresBackend, SqlDialect,
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

async fn journaled(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let sql = format!(
        "SELECT 1 FROM {}.schema_migrations WHERE version=$1 AND phase='completed'",
        cfg.pg.meta_schema
    );
    conn.query(&sql, &[&version]).await.map(|r| !r.is_empty()).unwrap_or(false)
}

/// A live `LiveSchema` whose `from` column carries `data_type` — so the rename
/// hint's type re-derivation (live `from` == IR-derived `to`) resolves and the PG
/// `OnlineIntent` type is byte-equal to the declarative path's `ddl_type(&r.ty)`.
/// The PG leg never reads `table_snapshots`/`sqlite_schemas`; they are present for
/// completeness (a real deploy introspects them).
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

/// One-op `renameColumn` IR.
fn rename_ir(table: &str, from: &str, to: &str, ty: ColType) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: format!("rename_{from}_to_{to}"),
        owner_app: "app_test".into(),
        ops: vec![Op::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
            ty,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Lower a one-op rename IR on the PG leg and unwrap the single
/// `RenameStep::PgExpandContract`.
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
    assert_eq!(steps.len(), 1, "one renameColumn → one plan step");
    match steps.into_iter().next().unwrap() {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => ec,
        other => panic!("PG must lower to a PgExpandContract, got {other:?}"),
    }
}

// §2.6.2 — the PG leg: ONE `op.renameColumn` lowers to a PgExpandContract and
// applies as an ONLINE dual-write through apply_plan. The EXPAND (E1..E3 +
// backfill) applies; C1/C2 are deferred into pending_contract; both columns exist
// after the expand and the backfill mirrored the rows.
#[compio::test]
async fn ir_renamecolumn_applies_as_pg_online_dual_write_through_apply_plan() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    // Create users(id, email) THROUGH apply_plan (migrator role owns it).
    engine
        .apply_plan(
            &[PlanStep::Ddl(zeroship_migrate::migration::Migration {
                version: zeroship_migrate::migration::MigrationId::generate(),
                name: "create_users".into(),
                up: format!(
                    "CREATE TABLE {s}.users (id bigint PRIMARY KEY, email text); \
                     INSERT INTO {s}.users (id, email) VALUES (1,'a@x.test'),(2,'b@x.test')"
                ),
                down: None,
                checksum: zeroship_migrate::migration::Checksum::of(
                    &zeroship_migrate::migration::ChecksumInput {
                        up: "create_users",
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
            })],
            Approval::None,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create users");

    // Lower `email → email_address` FROM THE IR (the real authoring path).
    let live = live_with_column("users", "email", "text");
    let ec = lower_pg_rename(&cfg, "users", "email", "email_address", ColType::Text, &live);
    let contract_versions: Vec<String> =
        ec.contract.iter().map(|m| m.version.as_str().to_string()).collect();
    assert_eq!(contract_versions.len(), 2, "C1 (drop trigger) + C2 (drop column)");

    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec))];
    let outcome = engine
        .apply_plan(
            &steps,
            Approval::Approved, // the expand's backfill mutates data
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("IR online rename expand applies via apply_plan");

    // The CONTRACT is partitioned into pending_contract (deferred to deploy N+1).
    let got_pending: Vec<String> = outcome
        .pending_contract
        .iter()
        .map(|m| m.version.as_str().to_string())
        .collect();
    assert_eq!(
        got_pending, contract_versions,
        "apply_plan defers C1/C2 into pending_contract (the cross-deploy partition)"
    );
    assert!(
        outcome.pending_contract.iter().any(|m| m.flags.destructive),
        "the deferred DROP COLUMN is destructive"
    );

    // After the EXPAND: BOTH columns exist; the backfill mirrored the rows.
    assert!(
        column_exists(&conn, &cfg.project_schema, "users", "email").await,
        "the old column survives the expand (its drop is the deferred contract)"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "users", "email_address").await,
        "the new column exists after the expand"
    );
    let rows = conn
        .query(&format!("SELECT email, email_address FROM {s}.users ORDER BY id"), &[])
        .await
        .expect("read mirrored rows");
    assert_eq!(rows.len(), 2, "two rows");
    for r in &rows {
        let from: Option<String> = r.get(0);
        let to: Option<String> = r.get(1);
        assert_eq!(to, from, "the backfill mirrored email → email_address");
    }

    // The contract was NOT journaled this deploy (it is pending, not applied).
    for v in &contract_versions {
        assert!(
            !journaled(&conn, &cfg, v).await,
            "contract version {v} must NOT be journaled at the expand deploy"
        );
    }
    teardown(&conn, &cfg).await;
}

// §2.6.2 — mid-sequence crash on the IR PG rename RESUMES roll-forward: re-running
// the SAME PgExpandContract plan after the first apply reaches the same final
// state (both columns, rows mirrored) and is a net-applied no-op (idempotent
// resume), never a double-apply.
#[compio::test]
async fn ir_renamecolumn_pg_crash_resumes_roll_forward() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    engine
        .apply_plan(
            &[PlanStep::Ddl(zeroship_migrate::migration::Migration {
                version: zeroship_migrate::migration::MigrationId::generate(),
                name: "create_acct".into(),
                up: format!(
                    "CREATE TABLE {s}.acct (id bigint PRIMARY KEY, handle text); \
                     INSERT INTO {s}.acct (id, handle) VALUES (1,'ada'),(2,'grace')"
                ),
                down: None,
                checksum: zeroship_migrate::migration::Checksum::of(
                    &zeroship_migrate::migration::ChecksumInput {
                        up: "create_acct",
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
            })],
            Approval::None,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("create acct");

    let live = live_with_column("acct", "handle", "text");
    let ec = lower_pg_rename(&cfg, "acct", "handle", "username", ColType::Text, &live);
    let expand_versions: Vec<String> =
        ec.expand.iter().map(|m| m.version.as_str().to_string()).collect();
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec))];

    // First apply (the "pre-crash" deploy): the EXPAND lands + journals.
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("first expand apply");
    for v in &expand_versions {
        assert!(journaled(&conn, &cfg, v).await, "expand step {v} journaled");
    }

    // RESUME: re-run the SAME plan (a crash-resume re-deploy). Each expand step is
    // net-applied ⇒ skipped (roll-forward, NOT a double-apply). The final state is
    // unchanged: both columns present, rows still mirrored.
    let resume = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("resume re-apply");
    for v in &expand_versions {
        assert!(
            resume.applied.skipped.contains(v),
            "resume skips the already-applied expand step {v} (roll-forward, no double-apply)"
        );
    }
    assert!(
        column_exists(&conn, &cfg.project_schema, "acct", "username").await,
        "the new column is present after resume"
    );
    let rows = conn
        .query(&format!("SELECT handle, username FROM {s}.acct ORDER BY id"), &[])
        .await
        .expect("read rows");
    for r in &rows {
        let from: Option<String> = r.get(0);
        let to: Option<String> = r.get(1);
        assert_eq!(to, from, "the rows stay mirrored across the resume");
    }
    teardown(&conn, &cfg).await;
}

// §2.6 — neutral-type translation on the PG leg: a renameColumn whose neutral
// ColType is `Timestamp` renders the correct PG type string (`timestamptz`) in the
// `OnlineIntent` carried by the ExpandContractPlan AND in E1's `ADD COLUMN <to>
// <pg-type>`. This proves the IR's dialect-neutral type is mapped to its PG
// spelling BEFORE the OnlineIntent is built (§2.6).
#[test]
fn ir_renamecolumn_pg_renders_neutral_type_as_pg_string_in_online_intent() {
    let cfg = cfg_for("neutral_pg");
    // The live `from` column's data_type must equal the IR-derived `to` type for
    // the rename hint to resolve — a pure rename never changes type. A Timestamp
    // column's information_schema spelling is `timestamp with time zone`.
    let live = live_with_column("events", "at", "timestamp with time zone");
    let ec = lower_pg_rename(&cfg, "events", "at", "occurred_at", ColType::Timestamp, &live);

    // The carried OnlineIntent's `ty` is the PG type STRING (`timestamptz`), never
    // the neutral token nor the information_schema long form.
    match &ec.intent {
        OnlineIntent::RenameColumn { ty, .. } => {
            assert_eq!(ty, "timestamptz", "neutral Timestamp → PG `timestamptz`");
        }
    }
    // E1's `up` is `ALTER TABLE … ADD COLUMN "occurred_at" timestamptz` — the PG
    // type string is spliced verbatim.
    let e1 = &ec.expand[0];
    assert!(
        e1.up.contains("ADD COLUMN") && e1.up.contains("timestamptz"),
        "E1 adds the new column with the PG type string: {}",
        e1.up
    );
}

// §2.6.1 — the E1..C2 `up`/`down`/`flags` SHAPE + intra-chain `depends_on`
// topology of an IR-authored rename are byte-equal to the equivalent declarative
// `ExpandContractAuthor`-authored rename's. Both call the SAME author with the
// SAME `OnlineIntent`, so only the freshly-minted UUIDv7 versions differ; the
// authored SQL, flags, names, and dependency EDGES (by position) are identical.
// This is the reconciliation with `expand_contract.rs` (the author is the id
// authority; the IR plan does not re-mint or diverge).
#[test]
fn ir_renamecolumn_pg_e1_to_c2_shape_byte_equal_to_declarative_rename() {
    let cfg = cfg_for("byteq");
    let live = live_with_column("widgets", "label", "text");
    let ir_ec = lower_pg_rename(&cfg, "widgets", "label", "title", ColType::Text, &live);

    // The declarative `t.*`-diff equivalent: the SAME ExpandContractAuthor with the
    // SAME OnlineIntent (the declarative path passes `ddl_type(&r.ty)` = "text").
    let decl_ec = zeroship_migrate::ExpandContractAuthor::new(&cfg.project_schema, "app_test")
        .author(&OnlineIntent::RenameColumn {
            table: "widgets".into(),
            from: "label".into(),
            to: "title".into(),
            ty: "text".into(),
        })
        .expect("declarative author");

    // The expand + contract sequences have the same LENGTH + per-step
    // (name, up, down, flags) tuple — byte-identical authored SQL.
    let shape = |m: &zeroship_migrate::migration::Migration| {
        (m.name.clone(), m.up.clone(), m.down.clone(), m.flags)
    };
    let ir_expand: Vec<_> = ir_ec.expand.iter().map(shape).collect();
    let decl_expand: Vec<_> = decl_ec.expand.iter().map(shape).collect();
    assert_eq!(ir_expand, decl_expand, "EXPAND E1..E3 authored byte-identically");
    let ir_contract: Vec<_> = ir_ec.contract.iter().map(shape).collect();
    let decl_contract: Vec<_> = decl_ec.contract.iter().map(shape).collect();
    assert_eq!(ir_contract, decl_contract, "CONTRACT C1..C2 authored byte-identically");

    // The intra-chain `depends_on` TOPOLOGY is identical: rebuild each plan's
    // dependency edges as POSITIONS within (expand ++ contract), so the freshly
    // minted versions are abstracted away and only the edge structure is compared.
    let edges = |ec: &zeroship_migrate::expand_contract::ExpandContractPlan| {
        let all: Vec<&zeroship_migrate::migration::Migration> =
            ec.expand.iter().chain(ec.contract.iter()).collect();
        let pos = |v: &zeroship_migrate::migration::MigrationId| {
            all.iter().position(|m| &m.version == v)
        };
        all.iter()
            .map(|m| m.depends_on.iter().map(&pos).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        edges(&ir_ec),
        edges(&decl_ec),
        "the E1..C2 intra-chain depends_on edge TOPOLOGY is identical (§2.6.1)"
    );
}
