//! **PR10 Part B** — faithful e2e for the executor-side existence-guard catalog
//! probe on REAL Postgres (`:5440`). Each test builds a guarded `op.*` IR, lowers
//! it through the REAL `IrAuthor` (which stamps the `GuardProbe` onto the lowered
//! `Migration`), and applies it through the REAL `apply_plan` orchestrator — the
//! probe runs under the held advisory lock + the open per-step txn. No shims, no
//! PG-gated skips: every assertion is against the real applied schema + the real
//! journal.
//!
//! Matrix (each RED on the pre-Part-B code, which REFUSED the lower fail-closed):
//! - addColumn ifNotExists: absent → runs; present+matching → SatisfiedNoop (no
//!   duplicate-column error, version STILL journaled); present+divergent type →
//!   `ExistenceGuardDrift` naming `data_type`, nothing applied.
//! - createTable ifNotExists: absent → creates; present+matching → no-op;
//!   present+extra-live-column → fail closed naming `columns`.
//! - createIndex ifNotExists: absent → create; present+matching → no-op;
//!   present+unique-flip → fail closed naming `unique`; invalid index residue
//!   from a crashed CONCURRENTLY build → recover/rebuild instead of no-op.
//! - addConstraint(unique) ifNotExists: absent → create; present same-name+same-kind
//!   → FailDrift naming `definition` (MED finding: NOT a silent noop);
//!   present same-name+DIFFERENT-kind → FailDrift naming `kind`.
//! - dropColumn/dropTable/dropIndex/dropConstraint ifExists: present → drops +
//!   journaled; absent → SatisfiedNoop journaled.

#![cfg(test)]

use compio_postgres::Client;
use zeroship_migrate::{
    model::ir::{
        ColType, ExistenceGuard, IrColumn, IrConstraint, IrConstraintKind, MigrationIr, Op,
        SelectAst, SelectItem, TableRef, ViewQuery,
    },
    render::lower::{IrAuthor, LiveSchema},
    ApplyError, Approval, EngineError,
    ExecutorConfig, MigrationEngine, PlanStep,
};
use zeroship_migrate::apply::role::deprovision_migrator;
use zeroship_migrate::{provision_migrator, record_started};
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
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column");
    !rows.is_empty()
}

async fn index_exists(conn: &Client, schema: &str, table: &str, index: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM pg_indexes \
             WHERE schemaname = $1 AND tablename = $2 AND indexname = $3",
            &[&schema, &table, &index],
        )
        .await
        .expect("query pg_indexes");
    !rows.is_empty()
}

async fn index_validity(conn: &Client, schema: &str, index: &str) -> Vec<bool> {
    conn
        .query(
            "SELECT x.indisvalid \
             FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
             ORDER BY c.relname",
            &[&schema, &index],
        )
        .await
        .expect("query index validity")
        .into_iter()
        .map(|r| r.get::<_, bool>("indisvalid"))
        .collect()
}

async fn inflight_exists(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let rows = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations_inflight WHERE version = $1",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("query inflight");
    !rows.is_empty()
}

async fn constraint_exists(conn: &Client, schema: &str, table: &str, constraint: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.table_constraints \
             WHERE constraint_schema = $1 AND table_name = $2 AND constraint_name = $3",
            &[&schema, &table, &constraint],
        )
        .await
        .expect("query table_constraints");
    !rows.is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table");
    !rows.is_empty()
}

async fn view_exists(conn: &Client, schema: &str, name: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')",
            &[&schema, &name],
        )
        .await
        .expect("query view");
    !rows.is_empty()
}

async fn journaled(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let rows = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations WHERE version = $1 AND phase = 'completed'",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("query journal");
    !rows.is_empty()
}

/// Lower a guarded IR op through the REAL `IrAuthor`, returning the plan steps
/// (with the `GuardProbe` stamped onto each `Migration`).
fn lower(cfg: &ExecutorConfig, op: Op) -> Vec<PlanStep> {
    let ir = MigrationIr {
        ir_version: 2,
        name: "guard_test".into(),
        owner_app: "app_test".into(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let author =
        IrAuthor::new(&cfg.project_schema, "app_test", SqlDialect::Postgres);
    let migs = author.lower(&ir, &LiveSchema::default()).expect("guarded op lowers");
    migs.into_iter().map(PlanStep::Ddl).collect()
}

async fn apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    steps: &[PlanStep],
) -> Result<(), EngineError> {
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            steps,
            Approval::Approved,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|e| match e {
            zeroship_migrate::DeclarativeApplyError::Plain(inner) => inner,
            zeroship_migrate::DeclarativeApplyError::Expand(inner) => {
                panic!("unexpected online-expand error in a guarded DDL test: {inner:?}")
            }
        })
}

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn { name: name.into(), ty, nullable: Some(true), default: None, unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }
}

/// Extract the `ExistenceGuardDrift` from a deep `EngineError`, or panic.
fn expect_drift(e: EngineError) -> (String, String, String) {
    match e {
        EngineError::Apply(ApplyError::ExistenceGuardDrift { object, field, expected, actual, .. }) => {
            (object, field, format!("{expected}|{actual}"))
        }
        other => panic!("expected ExistenceGuardDrift, got: {other:?}"),
    }
}

// --- addColumn ifNotExists -------------------------------------------------

#[compio::test]
async fn add_column_ifnotexists_absent_runs() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // base table without the guarded column
    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    })).await.expect("create base table");

    let steps = lower(&cfg, Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps).await.expect("guarded addColumn runs");
    assert!(column_exists(&conn, &cfg.project_schema, "t", "email").await, "column created");
    assert!(journaled(&conn, &cfg, &v).await, "version journaled");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn add_column_ifnotexists_present_matching_is_noop() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // pre-create the column with the SAME shape (text, nullable) the guard declares.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{s}\".t (id integer); ALTER TABLE \"{s}\".t ADD COLUMN email text",
        s = cfg.project_schema
    )).await.expect("pre-create table+column");

    let steps = lower(&cfg, Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    // Must NOT error (a bare ADD COLUMN would fail "column already exists").
    apply(&conn, &cfg, &steps).await.expect("guarded addColumn is a satisfied no-op");
    assert!(journaled(&conn, &cfg, &v).await, "satisfied no-op STILL journals the version");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn add_column_ifnotexists_present_divergent_type_fails_closed() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // pre-create the column with a DIVERGENT type (integer, not the declared text).
    conn.batch_execute(&format!(
        "CREATE TABLE \"{s}\".t (id integer); ALTER TABLE \"{s}\".t ADD COLUMN email integer",
        s = cfg.project_schema
    )).await.expect("pre-create divergent column");

    let steps = lower(&cfg, Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    let err = apply(&conn, &cfg, &steps).await.expect_err("divergent shape fails closed");
    let (object, field, _) = expect_drift(err);
    assert_eq!(field, "data_type", "names the diverging attribute");
    assert!(object.contains("email"), "names the column: {object}");
    assert!(!journaled(&conn, &cfg, &v).await, "nothing journaled on drift");
    teardown(&conn, &cfg).await;
}

// --- createTable ifNotExists ----------------------------------------------

#[compio::test]
async fn create_table_ifnotexists_present_extra_column_fails_closed() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // Create the declared table via the migrator (so the lowered snapshot's full
    // column set — incl. the auto-injected system `id` — exists), then add an EXTRA
    // live column out-of-band. The guarded re-create must fail closed: the live
    // table is WIDER than the declared shape.
    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    })).await.expect("create base table via migrator");
    conn.batch_execute(&format!(
        "ALTER TABLE \"{s}\".t ADD COLUMN sneaky text",
        s = cfg.project_schema
    )).await.expect("add extra live column");

    let steps = lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply(&conn, &cfg, &steps).await.expect_err("wider live table fails closed");
    let (_, field, _) = expect_drift(err);
    assert_eq!(field, "columns", "extra live column is a divergence");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn create_table_ifnotexists_present_matching_is_noop() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // Apply once (creates), then a guarded re-create over the matching shape no-ops.
    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    })).await.expect("create base table");

    let steps = lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps).await.expect("matching createTable is a satisfied no-op");
    assert!(journaled(&conn, &cfg, &v).await, "no-op journals the version");
    teardown(&conn, &cfg).await;
}

/// **C1 regression** — a guarded `createTable ifNotExists` lowers to MULTIPLE
/// units (CREATE TABLE + one CREATE INDEX per non-PK index [PG injects the 3
/// system-field indexes deleted_at/updated_at/created_by] + the `unique:true`
/// field's unique index). Before the fix the SAME `Table` probe was stamped on
/// EVERY unit, so once unit 0 created the table, units 1..N saw it PRESENT + base
/// columns matching → SatisfiedNoop → every secondary index was SILENTLY SKIPPED
/// (but journaled completed). This asserts the unique index AND the 3 system-field
/// indexes PHYSICALLY exist after a fresh guarded create, and that a RE-RUN of the
/// same guarded create is an idempotent no-op (no error, all units re-journaled).
///
/// RED pre-fix: the unique + system-field indexes are absent (skipped), so
/// `index_exists(... "t_email_key")` is false.
#[compio::test]
async fn create_table_ifnotexists_fresh_creates_all_secondary_indexes_and_reruns_idempotent() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let make_op = || Op::CreateTable {
        name: "t".into(),
        // a unique:true field → a `t_email_key` unique index unit, ON TOP of the
        // CREATE TABLE + the 3 PG system-field index units.
        columns: vec![IrColumn {
            name: "email".into(),
            ty: ColType::String,
            nullable: Some(true),
            default: None,
            unique: Some(true), id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    };

    // Fresh guarded create on an empty schema → unit0 creates the table; every
    // secondary index unit must ALSO run (its own object-scoped probe sees the
    // index ABSENT → RunBare), not be SatisfiedNoop'd by the table's presence.
    let steps = lower(&cfg, make_op());
    // > 1 unit: CREATE TABLE + unique idx + 3 system-field idxs.
    assert!(
        steps.len() >= 5,
        "a guarded createTable with a unique field + system fields lowers to ≥5 units; got {}",
        steps.len()
    );
    apply(&conn, &cfg, &steps).await.expect("fresh guarded create applies");

    let s = &cfg.project_schema;
    assert!(table_exists(&conn, s, "t").await, "table created");
    assert!(
        index_exists(&conn, s, "t", "t_email_key").await,
        "the unique:true field's unique index must PHYSICALLY exist (C1: it was silently skipped)"
    );
    for sysidx in ["t_deleted_at_idx", "t_updated_at_idx", "t_created_by_idx"] {
        assert!(
            index_exists(&conn, s, "t", sysidx).await,
            "the system-field index {sysidx} must PHYSICALLY exist (C1: it was silently skipped)"
        );
    }
    // Every unit's version must be journaled completed.
    for step in &steps {
        if let PlanStep::Ddl(m) = step {
            assert!(
                journaled(&conn, &cfg, m.version.as_str()).await,
                "unit {} journaled",
                m.version.as_str()
            );
        }
    }

    // RE-RUN: a fresh lowering of the SAME guarded create over the now-present
    // table+indexes must be an idempotent no-op — every unit SatisfiedNoops on its
    // OWN object (table present+matching; each index present+matching), with NO
    // "already exists" error.
    let steps2 = lower(&cfg, make_op());
    apply(&conn, &cfg, &steps2)
        .await
        .expect("re-run of the guarded create is an idempotent no-op");
    for step in &steps2 {
        if let PlanStep::Ddl(m) = step {
            assert!(
                journaled(&conn, &cfg, m.version.as_str()).await,
                "re-run unit {} re-journaled (satisfied no-op still journals)",
                m.version.as_str()
            );
        }
    }
    // Indexes are still present (the re-run did not drop/churn them).
    assert!(index_exists(&conn, s, "t", "t_email_key").await, "unique index survives re-run");
    teardown(&conn, &cfg).await;
}

// --- createIndex ifNotExists ----------------------------------------------

#[compio::test]
async fn create_index_ifnotexists_present_unique_flip_fails_closed() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // base table + a NON-unique index of the guarded name; the guard declares unique.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{s}\".t (id integer, email text); \
         CREATE INDEX t_email_idx ON \"{s}\".t (email)",
        s = cfg.project_schema
    )).await.expect("pre-create non-unique index");

    let steps = lower(&cfg, Op::CreateIndex {
        table: "t".into(),
        columns: vec!["email".into()],
        name: Some("t_email_idx".into()),
        unique: Some(true),
        using: None,
        r#where: None,
        concurrently: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply(&conn, &cfg, &steps).await.expect_err("unique flip fails closed");
    let (_, field, _) = expect_drift(err);
    assert_eq!(field, "unique");
    teardown(&conn, &cfg).await;
}

/// A crashed `CREATE INDEX CONCURRENTLY` leaves an INVALID index with the target
/// name plus a `started` marker. The guard probe must NOT count that invalid
/// catalog entry as "present+matching": it must return `RunBare`, allowing the
/// two-phase recovery path to drop the invalid residue and rebuild the index.
///
/// RED pre-fix: the shared snapshot included `indisvalid=false`, so the guard
/// returned `SatisfiedNoop`; `record_completed` cleared inflight and left the
/// invalid index permanently behind.
#[compio::test]
async fn create_index_ifnotexists_invalid_concurrent_residue_recovers_not_noops() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("email", ColType::String)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }))
    .await
    .expect("create base table via migrator");

    conn.batch_execute(&format!(
        "CREATE INDEX t_email_idx ON \"{s}\".t (email); \
         UPDATE pg_index SET indisvalid = false \
          WHERE indexrelid = '\"{s}\".t_email_idx'::regclass",
        s = cfg.project_schema
    ))
    .await
    .expect("seed invalid index residue");
    assert_eq!(
        index_validity(&conn, &cfg.project_schema, "t_email_idx").await,
        vec![false],
        "test setup must start with one invalid index"
    );

    let mut steps = lower(&cfg, Op::CreateIndex {
        table: "t".into(),
        columns: vec!["email".into()],
        name: Some("t_email_idx".into()),
        unique: Some(false),
        using: None,
        r#where: None,
        concurrently: Some(true),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let (version, name, checksum) = match &mut steps[0] {
        PlanStep::Ddl(m) => {
            m.flags.transactional = false;
            m.checksum =
                zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(m));
            assert!(
                !m.flags.transactional,
                "test setup must force transaction:false for the two-phase non-txn path"
            );
            (
                m.version.as_str().to_string(),
                m.name.clone(),
                m.checksum.as_str().to_string(),
            )
        }
        _ => unreachable!(),
    };
    record_started(
        &conn,
        &cfg,
        &version,
        &name,
        &checksum,
        "app_test",
    )
    .await
    .expect("seed crashed started marker");

    apply(&conn, &cfg, &steps)
        .await
        .expect("guarded concurrent index recovers and rebuilds");

    assert_eq!(
        index_validity(&conn, &cfg.project_schema, "t_email_idx").await,
        vec![true],
        "recovery must leave exactly one valid index"
    );
    assert!(journaled(&conn, &cfg, &version).await, "version journaled completed");
    assert!(
        !inflight_exists(&conn, &cfg, &version).await,
        "completed recovery must clear the started marker"
    );
    teardown(&conn, &cfg).await;
}

// --- addConstraint ifNotExists (MED finding) ------------------------------

#[compio::test]
async fn add_constraint_ifnotexists_present_same_kind_fails_definition() {
    // MED finding: a same-name + same-kind constraint is NOT a silent noop.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{s}\".t (id integer, email text); \
         ALTER TABLE \"{s}\".t ADD CONSTRAINT t_email_key UNIQUE (email)",
        s = cfg.project_schema
    )).await.expect("pre-create unique constraint");

    let steps = lower(&cfg, Op::AddConstraint {
        table: "t".into(),
        constraint: IrConstraint {
            name: Some("t_email_key".into()),
            kind: IrConstraintKind::Unique { columns: vec!["email".into()] },
        },
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply(&conn, &cfg, &steps).await.expect_err("same-name+same-kind fails closed");
    let (_, field, _) = expect_drift(err);
    assert_eq!(field, "definition", "MED: a same-kind constraint is refused, not noop'd");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn add_constraint_ifnotexists_present_different_kind_fails_kind() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // A PRIMARY KEY named t_email_key exists; the guard declares a UNIQUE of that name.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{s}\".t (id integer, email text NOT NULL); \
         ALTER TABLE \"{s}\".t ADD CONSTRAINT t_email_key PRIMARY KEY (email)",
        s = cfg.project_schema
    )).await.expect("pre-create PK with the guarded name");

    let steps = lower(&cfg, Op::AddConstraint {
        table: "t".into(),
        constraint: IrConstraint {
            name: Some("t_email_key".into()),
            kind: IrConstraintKind::Unique { columns: vec!["email".into()] },
        },
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply(&conn, &cfg, &steps).await.expect_err("kind clash fails closed");
    let (_, field, _) = expect_drift(err);
    assert_eq!(field, "kind");
    teardown(&conn, &cfg).await;
}

// --- F2: createTable deferred-FK re-run idempotency ------------------------

/// **F2 regression** — a guarded `createTable ifNotExists` carrying a DEFERRED FK
/// (target not yet live at lower → emitted as a follow-on `ALTER TABLE … ADD
/// CONSTRAINT … FOREIGN KEY …` unit). Before the fix the createTable FK unit reused
/// the stand-alone `addConstraint` fail-closed rule, so on a RE-DEPLOY the present
/// same-name+same-kind FK hard-FailDrifted (`ExistenceGuardDrift { constraint
/// owner_fkey, definition }`) — any forward/cyclic-reference guarded create was
/// non-idempotent. The fix stamps the byte-comparable `pg_get_constraintdef`-shaped
/// `expect_definition` on the createTable FK probe, so a re-run that finds a
/// byte-equal live FK is a SatisfiedNoop.
///
/// RED pre-fix: the second apply returns `ExistenceGuardDrift { definition }` on
/// `owner_fkey`.
#[compio::test]
async fn create_table_ifnotexists_deferred_fk_reruns_idempotent() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // The FK target table must physically exist at apply time (the deferred ADD
    // CONSTRAINT references it) AND be OWNED by the migrator role so the least-priv
    // ADD CONSTRAINT has REFERENCES privilege — so create it THROUGH the migrator.
    // Each `lower()` uses an empty LiveSchema, so `people` is never live at the
    // `pets` lowering and the `ref` ALWAYS lowers as a DEFERRED FK (a follow-on ALTER).
    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "people".into(),
        columns: vec![col("label", ColType::String)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }))
    .await
    .expect("create FK target table via the migrator");

    // A guarded createTable with a `ref` to `people` → a deferred FK unit
    // (`owner_fkey`) on top of the CREATE TABLE + system-field index units.
    let make_op = || Op::CreateTable {
        name: "pets".into(),
        columns: vec![IrColumn {
            name: "owner".into(),
            ty: ColType::Ref { references: "people".into() },
            nullable: Some(true),
            default: None,
            unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    };

    // First apply: creates pets + the deferred FK.
    let steps1 = lower(&cfg, make_op());
    apply(&conn, &cfg, &steps1).await.expect("fresh guarded create with deferred FK applies");
    let s = &cfg.project_schema;
    assert!(table_exists(&conn, s, "pets").await, "pets created");
    assert!(
        constraint_exists(&conn, s, "pets", "owner_fkey").await,
        "the deferred FK owner_fkey must PHYSICALLY exist after the fresh create"
    );

    // RE-RUN: the deferred-FK unit's probe finds a present same-name FK whose live
    // `pg_get_constraintdef` byte-equals the declared definition → SatisfiedNoop, NOT
    // a hard FailDrift. The whole guarded create re-runs idempotently.
    let steps2 = lower(&cfg, make_op());
    apply(&conn, &cfg, &steps2)
        .await
        .expect("re-run of the guarded createTable with a deferred FK is idempotent (F2)");
    assert!(
        constraint_exists(&conn, s, "pets", "owner_fkey").await,
        "the FK survives the idempotent re-run"
    );
    teardown(&conn, &cfg).await;
}

// --- ifExists drops --------------------------------------------------------

#[compio::test]
async fn drop_column_ifexists_present_runs_absent_noops() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // Create the base table via the migrator so the guarded dropColumn (RunBare —
    // it actually executes ALTER under SET ROLE migrator) is the table OWNER.
    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("legacy", ColType::String)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    })).await.expect("create base table via migrator");

    // present → drops + journals.
    let steps = lower(&cfg, Op::DropColumn {
        table: "t".into(),
        column: "legacy".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps).await.expect("drop present column runs");
    assert!(!column_exists(&conn, &cfg.project_schema, "t", "legacy").await, "column dropped");
    assert!(journaled(&conn, &cfg, &v).await);

    // absent → SatisfiedNoop (a fresh version over the now-absent column).
    let steps2 = lower(&cfg, Op::DropColumn {
        table: "t".into(),
        column: "legacy".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v2 = match &steps2[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps2).await.expect("drop absent column is a satisfied no-op");
    assert!(journaled(&conn, &cfg, &v2).await, "satisfied no-op journals");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn drop_table_ifexists_absent_noops() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    // No table exists; dropTable ifExists → SatisfiedNoop, still journaled.
    let steps = lower(&cfg, Op::DropTable {
        table: "ghost".into(),
        cascade: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps).await.expect("drop absent table is a satisfied no-op");
    assert!(!table_exists(&conn, &cfg.project_schema, "ghost").await);
    assert!(journaled(&conn, &cfg, &v).await, "satisfied no-op journals");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn drop_view_ifexists_present_runs_absent_noops() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    apply(&conn, &cfg, &lower(&cfg, Op::CreateTable {
        name: "t".into(),
        columns: vec![col("name", ColType::String)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    })).await.expect("create base table via migrator");

    apply(&conn, &cfg, &lower(&cfg, Op::CreateView {
        name: "active_v".into(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: SelectAst {
                from: TableRef { name: "t".into(), schema: None, alias: None },
                projection: vec![SelectItem::ColRef {
                    table: None,
                    name: "name".into(),
                    alias: None,
                }],
                joins: vec![],
                r#where: None,
                order_by: None,
                limit: None,
            },
        },
        replace: None,
        materialized: None,
    })).await.expect("create view via migrator");
    assert!(view_exists(&conn, &cfg.project_schema, "active_v").await, "view created");

    let steps = lower(&cfg, Op::DropView {
        name: "active_v".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
        materialized: None,
    });
    let v = match &steps[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps).await.expect("drop present view runs");
    assert!(!view_exists(&conn, &cfg.project_schema, "active_v").await, "view dropped");
    assert!(journaled(&conn, &cfg, &v).await, "present drop journals");

    let steps2 = lower(&cfg, Op::DropView {
        name: "active_v".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
        materialized: None,
    });
    let v2 = match &steps2[0] { PlanStep::Ddl(m) => m.version.as_str().to_string(), _ => unreachable!() };
    apply(&conn, &cfg, &steps2).await.expect("drop absent view is a satisfied no-op");
    assert!(!view_exists(&conn, &cfg.project_schema, "active_v").await, "view remains absent");
    assert!(journaled(&conn, &cfg, &v2).await, "satisfied no-op journals");
    teardown(&conn, &cfg).await;
}
