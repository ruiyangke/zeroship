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
use zeroship_migrate::{ColumnSnapshot, TableSnapshot};
use zeroship_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::render::lower::{IrAuthor, IrLowerError, LiveSchema};
use zeroship_migrate::{PlanStep, RenameStep};
use zeroship_migrate::{
    provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig, MigrationEngine,
    ExpandContractPlan, OnlineIntent, PostgresBackend, SqlDialect,
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
            ddl_type_override: None,
            inline_checks: Vec::new(),
            generated: None,
            identity: None,
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

/// Lower a one-op rename IR on the PG leg and unwrap the single
/// `RenameStep::PgExpandContract`.
fn lower_pg_rename(
    cfg: &ExecutorConfig,
    table: &str,
    from: &str,
    to: &str,
    ty: ColType,
    live: &LiveSchema,
) -> ExpandContractPlan {
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
            &[PlanStep::Ddl(zeroship_migrate::model::migration::Migration {
                version: zeroship_migrate::model::migration::MigrationId::generate(),
                name: "create_users".into(),
                up: format!(
                    "CREATE TABLE {s}.users (id bigint PRIMARY KEY, email text); \
                     INSERT INTO {s}.users (id, email) VALUES (1,'a@x.test'),(2,'b@x.test')"
                ),
                down: None,
                checksum: zeroship_migrate::model::migration::Checksum::of(
                    &zeroship_migrate::model::migration::ChecksumInput {
                        up: "create_users",
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
            })],
            Approval::None,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
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
            zeroship_migrate::apply::executor::LockMode::Acquire,
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

// PR2-LOW — EXECUTE the reversal of a FULLY-APPLIED online rename (contract→expand
// reversal, §2.6.1). A pure rename's auto-derived `down` is a FRESH online rename
// `to`→`from`; this test proves that round-trip executes on a real DB. Apply rename
// `email → email_address` (EXPAND under approval), then APPLY the deferred CONTRACT
// (drop `email`) so the rename is fully applied (only `email_address` remains), then
// apply the REVERSE rename `email_address → email` (the auto-derived down shape) and
// assert `email` is back with the mirrored data. This is the engine-level
// counterpart to a `down()` that calls `renameColumn(to, from)`.
#[compio::test]
async fn ir_renamecolumn_pg_fully_applied_rename_reverses_via_inverse_rename() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();
    let pg_be = PostgresBackend::new(&conn);

    // create members(id, email) + seed.
    engine
        .apply_plan(
            &[PlanStep::Ddl(zeroship_migrate::model::migration::Migration {
                version: zeroship_migrate::model::migration::MigrationId::generate(),
                name: "create_members".into(),
                up: format!(
                    "CREATE TABLE {s}.members (id bigint PRIMARY KEY, email text); \
                     INSERT INTO {s}.members (id, email) VALUES (1,'ada@x.test'),(2,'gr@x.test')"
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
            })],
            Approval::None,
            &pg_be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("create members");

    // 1) EXPAND: rename email → email_address.
    let live1 = live_with_column("members", "email", "text");
    let ec = lower_pg_rename(&cfg, "members", "email", "email_address", ColType::Text, &live1);
    let contract = ec.contract.clone();
    let expand_steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec))];
    engine
        .apply_plan(&expand_steps, Approval::Approved, &pg_be, &cfg, "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire)
        .await
        .expect("expand applies");

    // 2) CONTRACT (deploy N+1): apply C1/C2 (drop trigger + drop old `email`) so the
    //    rename is FULLY applied. The contract migrations are plain DDL with the
    //    expand dep already satisfied (journaled), so apply them as Ddl steps.
    let contract_steps: Vec<PlanStep> =
        contract.into_iter().map(PlanStep::Ddl).collect();
    engine
        .apply_plan(&contract_steps, Approval::Approved, &pg_be, &cfg, "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire)
        .await
        .expect("contract applies (fully-applied rename)");
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "after the contract, the old `email` column is gone (fully-applied rename)"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email_address").await,
        "only the new `email_address` column remains"
    );

    // 3) REVERSAL — the auto-derived down: a FRESH online rename email_address →
    //    email. EXPAND it under approval.
    let live2 = live_with_column("members", "email_address", "text");
    let rev = lower_pg_rename(&cfg, "members", "email_address", "email", ColType::Text, &live2);
    let rev_contract = rev.contract.clone();
    let rev_steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(rev))];
    engine
        .apply_plan(&rev_steps, Approval::Approved, &pg_be, &cfg, "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire)
        .await
        .expect("reverse rename expand applies");
    // Complete the reverse contract (drop email_address).
    let rev_contract_steps: Vec<PlanStep> =
        rev_contract.into_iter().map(PlanStep::Ddl).collect();
    engine
        .apply_plan(&rev_contract_steps, Approval::Approved, &pg_be, &cfg, "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire)
        .await
        .expect("reverse contract applies");

    // The reversal restored `email` with the original data, and `email_address` is
    // gone — the fully-applied rename round-tripped via the inverse rename.
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "email").await,
        "the reversal restored the original `email` column"
    );
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "email_address").await,
        "the reversal dropped the renamed `email_address` column"
    );
    let rows = conn
        .query(&format!("SELECT email FROM {s}.members ORDER BY id"), &[])
        .await
        .expect("read restored email");
    let vals: Vec<Option<String>> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(
        vals,
        vec![Some("ada@x.test".to_string()), Some("gr@x.test".to_string())],
        "the original data survived the rename round-trip"
    );

    teardown(&conn, &cfg).await;
}

// §2.6.2 — mid-sequence crash on the IR PG rename RESUMES roll-forward. This arms
// the REAL fault seam (`EXPAND_BETWEEN_E2_AND_BACKFILL`) on the IR-LOWERED rename
// plan (NOT a hand-built ExpandContractPlan): the first apply CRASHES between the
// E2 trigger and the E3 backfill (E1/E2 journaled, E3 not), then a fault-free
// resume converges to the same final state (both columns, rows mirrored) without
// double-journaling. This proves a genuine mid-sequence crash recovery on the IR
// path, not merely idempotent full-replay.
//
// MED (code-critic): the prior version of this test injected NO crash — it
// applied the whole expand then re-applied the same plan and asserted full-replay
// skips. That never armed the fault seam, so it did not exercise a mid-sequence
// (E2→backfill) crash on the IR-lowered plan. This version arms the seam.
#[compio::test]
async fn ir_renamecolumn_pg_crash_resumes_roll_forward() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);
    let engine = MigrationEngine::new();

    // Make sure no fault leaks in from a prior test in this binary.
    zeroship_migrate::fault::disarm_all();

    engine
        .apply_plan(
            &[PlanStep::Ddl(zeroship_migrate::model::migration::Migration {
                version: zeroship_migrate::model::migration::MigrationId::generate(),
                name: "create_acct".into(),
                up: format!(
                    "CREATE TABLE {s}.acct (id bigint PRIMARY KEY, handle text); \
                     INSERT INTO {s}.acct (id, handle) VALUES (1,'ada'),(2,'grace')"
                ),
                down: None,
                checksum: zeroship_migrate::model::migration::Checksum::of(
                    &zeroship_migrate::model::migration::ChecksumInput {
                        up: "create_acct",
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
            })],
            Approval::None,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("create acct");

    let live = live_with_column("acct", "handle", "text");
    let ec = lower_pg_rename(&cfg, "acct", "handle", "username", ColType::Text, &live);
    let expand_versions: Vec<String> =
        ec.expand.iter().map(|m| m.version.as_str().to_string()).collect();
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(ec))];

    // ARM the crash between E2 (dual-write trigger installed) and the E3 backfill.
    // The first apply MUST fail at that boundary — E1/E2 land + journal, E3 does
    // not. This is a genuine mid-sequence crash on the IR-lowered plan.
    zeroship_migrate::fault::arm(
        zeroship_migrate::fault::points::EXPAND_BETWEEN_E2_AND_BACKFILL,
        0,
    );
    let crashed = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await;
    assert!(
        crashed.is_err(),
        "the armed E2→backfill fault MUST fire on the first apply (else the resume \
         below proves nothing); got {crashed:?}"
    );
    zeroship_migrate::fault::disarm_all();

    // RESUME (fault-free): the rename converges. E1/E2 are net-applied ⇒ skipped;
    // the E3 backfill (which the crash skipped) now runs and mirrors the rows.
    let resume = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &PostgresBackend::new(&conn),
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("resume after the simulated crash must converge");
    // The resume's applied+skipped tally must cover the whole expand (E1/E2 were
    // journaled by the crashed first apply ⇒ skipped; E3 is freshly applied), and
    // the crash boundary (E2→backfill) means at least one expand step actually
    // re-applies on resume — this is roll-forward, not a vacuous no-op replay.
    let touched: std::collections::BTreeSet<&String> =
        resume.applied.applied.iter().chain(resume.applied.skipped.iter()).collect();
    for v in &expand_versions {
        assert!(
            journaled(&conn, &cfg, v).await,
            "expand step {v} is journaled completed after the resume"
        );
        assert!(
            touched.contains(v),
            "the resume tally accounts for expand step {v} (applied or skipped)"
        );
    }
    assert!(
        !resume.applied.applied.is_empty(),
        "the resume re-applied the crash-skipped E3 backfill (roll-forward), not a \
         pure no-op replay"
    );
    // PR2-LOW — the converged expand journal has NO duplicate `completed` row per
    // version (a resume must not DOUBLE-JOURNAL a step — an illegal state-machine
    // transition). The prior assertion dedup'd a STATICALLY-built `Vec` (the lowered
    // plan's version list), which can NEVER have duplicates and so proved nothing
    // about the journal. This QUERIES the real append-only journal: for each expand
    // version, COUNT(*) of `completed` rows must be EXACTLY 1 — total == distinct.
    // A double-journal (the bug this guards) would insert a second `completed` row
    // and make the count 2.
    let meta = q(&cfg.pg.meta_schema);
    for v in &expand_versions {
        let rows = conn
            .query(
                &format!(
                    "SELECT count(*) FROM {meta}.schema_migrations \
                     WHERE version=$1 AND phase='completed'"
                ),
                &[v],
            )
            .await
            .expect("count completed journal rows for the expand version");
        let count: i64 = rows[0].get(0);
        assert_eq!(
            count, 1,
            "expand version {v} must have EXACTLY ONE completed journal row after the \
             crash+resume (a double-journal would make this 2)"
        );
    }

    assert!(
        column_exists(&conn, &cfg.project_schema, "acct", "username").await,
        "the new column is present after the crash+resume"
    );
    let rows = conn
        .query(&format!("SELECT handle, username FROM {s}.acct ORDER BY id"), &[])
        .await
        .expect("read rows");
    assert_eq!(rows.len(), 2, "two rows");
    for r in &rows {
        let from: Option<String> = r.get(0);
        let to: Option<String> = r.get(1);
        assert_eq!(
            to, from,
            "the E3 backfill ran on resume and mirrored handle → username"
        );
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

// PR2-LOW — an IN-FLIGHT OnlineRename makes the PLAN non-rollbackable and the
// plan-level rollback driver hard-stops. A PG expand-contract is a multi-phase
// online change with a dual-write trigger and a cross-deploy partition: mid-flight
// it has no single-statement inverse, so recovery is roll-FORWARD, not roll-back
// (§2.6.1). The contract is expressed at the PLAN level: `PlanStep::has_down` is
// `false` for an `OnlineRename` step regardless of the inner E1..E3 migrations'
// individual `down`s (those are sub-step DDLs of an atomically-driven sequence, not
// independently rollbackable phases), so `AppliedPlan::compute_rollbackable` over a
// plan containing the OnlineRename step is `false` — the plan-level rollback driver
// hard-stops at it rather than fabricating a partial reverse. A pure-lowering fact
// (no DB).
#[test]
fn ir_renamecolumn_pg_in_flight_rename_is_not_rollbackable() {
    use zeroship_migrate::AppliedPlan;
    let cfg = cfg_for("inflight_rollback_pg");
    let live = live_with_column("users", "email", "text");
    let ec = lower_pg_rename(&cfg, "users", "email", "email_address", ColType::Text, &live);

    let step = PlanStep::OnlineRename(RenameStep::PgExpandContract(ec));
    // The OnlineRename STEP has no plan-level `down` — the rollback driver treats it
    // as irreversible-in-place (roll-forward only).
    assert!(
        !step.has_down(),
        "an OnlineRename plan step must report has_down() == false (no plan-level \
         single-statement inverse mid-flight)"
    );

    // A plan containing the OnlineRename step is therefore NON-rollbackable.
    let steps = vec![step];
    assert!(
        !AppliedPlan::compute_rollbackable(&steps),
        "a plan with an in-flight OnlineRename step must be rollbackable:false (the \
         rollback driver hard-stops at it)"
    );
}

// HIGH (code-critic) — IR-vs-live type reconciliation on the PG leg. A
// `renameColumn` whose IR-carried `ColType` DISAGREES with the live `from`
// column's actual type MUST fail closed (`RenameTypeMismatch`) — the IR-path
// mirror of the declarative differ's `RenameHintTypeMismatch`. A pure rename
// mirrors values across the two columns (the dual-write `NEW.<to> := NEW.<from>`)
// and cannot also change the type; trusting a wrong IR `ty` (here `Int` over a
// live `text` column) would author a mismatched `ADD COLUMN <to> integer` + a
// cross-type dual-write copy with no rejection. Pre-fix the PG leg derived the
// type SOLELY from the IR `ColType` and never read the live column, so this
// lowered successfully (the load-bearing HIGH defect). This is a pure-lowering
// fact (no DB needed): the reconciliation runs before any author.
#[test]
fn ir_renamecolumn_pg_rejects_ir_type_disagreeing_with_live_column() {
    let cfg = cfg_for("type_mismatch_pg");
    // The live `email` column is `text`; the IR claims the renamed column is `Int`.
    let live = live_with_column("users", "email", "text");
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = rename_ir("users", "email", "addr", ColType::Int);
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a rename whose IR type disagrees with the live column must fail closed");
    match err {
        IrLowerError::RenameTypeMismatch { table, from, to, live_type, .. } => {
            assert_eq!(table, "users");
            assert_eq!(from, "email");
            assert_eq!(to, "addr");
            assert_eq!(live_type, "text", "the live `from` type is the authority");
        }
        other => panic!("expected RenameTypeMismatch, got: {other}"),
    }
}

// PR2-LOW — rename-to-EXISTING-column collision (PG leg). A `renameColumn` whose
// `to` equals a column that ALREADY exists on the live table must fail closed at the
// LOWER gate (`RenameLower`), NOT lower to an `ADD COLUMN <to>` that fails late at
// apply with an opaque "column already exists". The type reconciliation only checks
// `from`; this guard checks `to`. Pre-fix the PG leg never consulted `to` against the
// live columns, so the collision lowered successfully and failed only at apply.
#[test]
fn ir_renamecolumn_pg_rejects_rename_to_existing_column() {
    let cfg = cfg_for("rename_to_existing_pg");
    // The live `accounts` table has BOTH `email` (the from) and `addr` (the to)
    // columns — both `text`.
    let snap = TableSnapshot {
        columns: vec![
            ColumnSnapshot {
                name: "email".into(),
                data_type: "text".into(),
                nullable: true,
                default: None,
                ddl_type_override: None,
                inline_checks: Vec::new(),
                generated: None,
                identity: None,
                encryption_sentinel: None,
                comment_sentinel: None,
            },
            ColumnSnapshot {
                name: "addr".into(),
                data_type: "text".into(),
                nullable: true,
                default: None,
                ddl_type_override: None,
                inline_checks: Vec::new(),
                generated: None,
                identity: None,
                encryption_sentinel: None,
                comment_sentinel: None,
            },
        ],
        indexes: vec![],
        constraints: vec![],
        stored_create_sql: None,
    };
    let mut live = LiveSchema::default();
    live.tables.insert("accounts".into());
    live.table_snapshots.insert("accounts".into(), snap);

    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = rename_ir("accounts", "email", "addr", ColType::Text);
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a rename whose `to` collides with an existing live column must fail closed");
    match err {
        IrLowerError::RenameLower(msg) => {
            assert!(
                msg.contains("addr") && msg.contains("already exists"),
                "the collision error must name the offending `to` column: {msg}"
            );
        }
        other => panic!("expected RenameLower (to-collision), got: {other}"),
    }
}

// HIGH (code-critic) — the live `from` column structure is MANDATORY for a rename
// on the PG leg: without it the IR-vs-live type reconciliation cannot run, so the
// lowering fails closed (`RenameNeedsLiveColumn`) rather than trusting the
// IR-carried type alone. Pre-fix the PG leg needed no live structure at all and
// happily authored from `{from,to,ty}`.
#[test]
fn ir_renamecolumn_pg_fails_closed_without_live_from_column() {
    let cfg = cfg_for("no_live_col_pg");
    // LiveSchema knows the table name but carries NO column structure.
    let mut live = LiveSchema::default();
    live.tables.insert("users".into());
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = rename_ir("users", "email", "addr", ColType::Text);
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a PG rename with no live `from` column must fail closed");
    match err {
        IrLowerError::RenameNeedsLiveColumn(t, c) => {
            assert_eq!(t, "users");
            assert_eq!(c, "email");
        }
        other => panic!("expected RenameNeedsLiveColumn, got: {other}"),
    }
}

// LOW (code-critic) — the IR-vs-live type-gate must be ROUND-TRIP-SYMMETRIC for a
// PARAMETERISED EXTENSION type (`vector(N)`), not just the 9 base types. The live
// `from` column introspects to `vector(3)` (PG `format_type` → `canonical_extension_type`
// keeps `vector(N)` verbatim); the IR-derived `to` type comes from the SHARED
// snapshot builder over `ColType::Vector { vector: 3 }`. The gate compares the two
// `data_type` strings for EXACT equality, so if the IR path were to DROP the `(N)`
// dimension (spell a dimensionless `vector`) a legitimate vector rename would
// false-reject with `RenameTypeMismatch`. This test pins that the IR derives
// `vector(3)` (carrying the dimension through `ir_column_to_field`), so the gate
// PASSES and the rename lowers to a `PgExpandContract` whose E1 `ADD COLUMN`
// carries the dimensioned `vector(3)` type — proving the extension-type round-trip
// for the RENAME path, not only for plain createTable snapshots. Pure lowering (no
// DB / no pgvector extension needed): the type-gate runs before any author.
#[test]
fn ir_renamecolumn_pg_vector_extension_type_gate_round_trips() {
    let cfg = cfg_for("vector_gate_pg");
    // The live `embedding` column introspects to the canonical `vector(3)` spelling
    // (exactly what `apply::drift::canonical_extension_type` recovers from `format_type`).
    let live = live_with_column("docs", "embedding", "vector(3)");
    let ec = lower_pg_rename(
        &cfg,
        "docs",
        "embedding",
        "vec",
        ColType::Vector { vector: 3 },
        &live,
    );
    // The reconciled type carried into the OnlineIntent (and E1's ADD COLUMN) keeps
    // the `(3)` dimension — the extension-type round-trip is byte-symmetric.
    match &ec.intent {
        OnlineIntent::RenameColumn { ty, .. } => {
            assert_eq!(
                ty, "vector(3)",
                "the vector rename keeps its dimension (round-trip symmetric with live)"
            );
        }
    }
    let e1 = &ec.expand[0];
    assert!(
        e1.up.contains("ADD COLUMN") && e1.up.contains("vector(3)"),
        "E1 adds the new column with the dimensioned vector type: {}",
        e1.up
    );
}

// LOW (code-critic) — the companion NEGATIVE proof: a vector rename whose live
// `from` column has a DIFFERENT dimension (`vector(4)`) than the IR asserts
// (`vector: 3`) MUST fail closed with `RenameTypeMismatch`. This confirms the
// extension-type gate is a real reconciliation (it actually compares the
// dimensions), not a vacuous pass that would let a mismatched vector rename
// through.
#[test]
fn ir_renamecolumn_pg_vector_dimension_mismatch_fails_closed() {
    let cfg = cfg_for("vector_mismatch_pg");
    let live = live_with_column("docs", "embedding", "vector(4)");
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = rename_ir("docs", "embedding", "vec", ColType::Vector { vector: 3 });
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a vector rename whose dimension disagrees with live must fail closed");
    match err {
        IrLowerError::RenameTypeMismatch { live_type, ir_type, .. } => {
            assert_eq!(live_type, "vector(4)", "live dimension is the authority");
            assert_eq!(ir_type, "vector(3)", "the IR-asserted dimension is carried, then rejected");
        }
        other => panic!("expected RenameTypeMismatch, got: {other}"),
    }
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
    let shape = |m: &zeroship_migrate::model::migration::Migration| {
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
    let edges = |ec: &ExpandContractPlan| {
        let all: Vec<&zeroship_migrate::model::migration::Migration> =
            ec.expand.iter().chain(ec.contract.iter()).collect();
        let pos = |v: &zeroship_migrate::model::migration::MigrationId| {
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
