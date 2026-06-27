//! PR9b — PER-VERSION APPROVAL SCOPING (anti-bypass), faithful e2e on REAL Postgres
//! (`:5440`). No shims, no PG-gated skips.
//!
//! These pin the security guarantee that approving ONE reviewed destructive op can
//! NEVER blanket-authorize an UNRELATED co-bundled destructive op the operator never
//! individually reviewed:
//!
//! - `approving_rename_version_does_not_authorize_co_bundled_dropcolumn` — a plan
//!   carrying an approved online rename at version V AND a co-bundled `dropColumn` at
//!   version W, scoped to `{V}`, is REFUSED (`ApprovalNotScoped { version: W }`) and
//!   applies NOTHING (RED pre-fix: a blanket `Approval::Approved` would have applied
//!   both).
//! - `approving_the_exact_destructive_version_authorizes_only_that_op` — a lone
//!   `dropColumn` at W applies under scope `{W}` and is REFUSED under scope `{V}`.
//! - `unscoped_empty_scope_authorizes_no_destructive_op` — scope `{}` under
//!   `Approval::Approved` refuses any destructive op (fail-closed).
//! - `approval_scope_all_preserves_legacy_blanket_behavior` — `ApprovalScope::All` +
//!   `Approval::Approved` applies the destructive op (byte-identical to pre-PR9b),
//!   pinning the no-regression default.
//!
//! Every assertion is against the real applied schema + the real journal.

use compio_postgres::Client;
use zeroship_migrate::{ColumnSnapshot, TableSnapshot};
use zeroship_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::render::lower::{IrAuthor, LiveSchema};
use zeroship_migrate::PlanStep;
use zeroship_migrate::{
    provision_migrator, apply::role::deprovision_migrator, ApplyError, Approval, ApprovalScope,
    DeclarativeApplyError, EngineError, ExecutorConfig, MigrationEngine, PostgresBackend,
    SqlDialect,
};

/// The per-version scope refusal can surface from EITHER gate (both fail-closed,
/// defense-in-depth): the engine's `EngineError::ApprovalNotScoped` (rebuild / DML /
/// PG EXPAND arms) OR the executor's `ApplyError::ApprovalNotScoped` (the coalesced
/// DDL batch gate). A DDL `dropColumn` hits the executor gate. Returns the refused
/// version, asserting it equals `want` when provided.
fn assert_not_scoped(err: &DeclarativeApplyError, want: Option<&str>) {
    let version = match err {
        DeclarativeApplyError::Plain(EngineError::ApprovalNotScoped { version }) => version.clone(),
        DeclarativeApplyError::Plain(EngineError::Apply(ApplyError::ApprovalNotScoped {
            version,
        })) => version.clone(),
        other => panic!("expected an ApprovalNotScoped refusal, got {other:?}"),
    };
    if let Some(want) = want {
        assert_eq!(version, want, "the refused version must be the un-scoped op's version");
    }
}

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

async fn journal_count(conn: &Client, cfg: &ExecutorConfig) -> i64 {
    // The meta journal table may not exist if nothing ever applied — treat absent
    // as zero (the read-back ensure_journal creates it on a real apply).
    let row = conn
        .query_one(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema=$1 AND table_name='schema_migrations'",
            &[&cfg.pg.meta_schema],
        )
        .await
        .expect("count meta table");
    if row.get::<_, i64>(0) == 0 {
        return 0;
    }
    let row = conn
        .query_one(
            &format!("SELECT count(*) FROM {}.schema_migrations", q(&cfg.pg.meta_schema)),
            &[],
        )
        .await
        .expect("count journal rows");
    row.get(0)
}

fn live_members() -> LiveSchema {
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
                name: "legacy".into(),
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
    live.tables.insert("members".into());
    live.table_snapshots.insert("members".into(), snap);
    live
}

/// Lower a one-op `dropColumn` IR on the PG leg to its destructive `Ddl` step. With
/// PR9b the version is the DETERMINISTIC `ddl_step_version` (a stable, reviewable id),
/// so the returned step's version is the scope key.
fn drop_column_step(cfg: &ExecutorConfig, table: &str, column: &str, live: &LiveSchema) -> PlanStep {
    let author = IrAuthor::new(cfg.project_schema.clone(), "app_test", SqlDialect::Postgres);
    let ir = MigrationIr {
        ir_version: 1,
        name: format!("drop_{column}"),
        owner_app: "app_test".into(),
        ops: vec![Op::DropColumn {
            table: table.into(),
            column: column.into(),
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let steps = author.lower_steps(&ir, live).expect("dropColumn lowers");
    steps.into_iter().next().expect("one step")
}

/// Lower a one-op rename IR on the PG leg to its `PgExpandContract` step.
fn rename_step(cfg: &ExecutorConfig, table: &str, from: &str, to: &str, live: &LiveSchema) -> PlanStep {
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
    let steps = author.lower_steps(&ir, live).expect("rename lowers");
    steps.into_iter().next().expect("one step")
}

/// `CREATE TABLE members(id, email, legacy)` + seed two rows, via a plain Ddl plan.
async fn create_members(engine: &MigrationEngine, be: &PostgresBackend<'_>, cfg: &ExecutorConfig) {
    let s = q(&cfg.project_schema);
    let up = format!(
        "CREATE TABLE {s}.members (id bigint PRIMARY KEY, email text, legacy text); \
         INSERT INTO {s}.members (id, email, legacy) VALUES (1,'a@x.test','x'),(2,'b@x.test','y')"
    );
    let m = zeroship_migrate::model::migration::Migration {
        version: zeroship_migrate::model::migration::MigrationId::generate(),
        name: "create_members".into(),
        up,
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
            &[PlanStep::Ddl(m)],
            Approval::None,
            be,
            cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("create members");
}

fn scope_version(step: &PlanStep) -> String {
    step.approval_scope_version()
        .expect("a scope-gated step must expose a scope version")
        .to_string()
}

// (1) Approving the RENAME version V does NOT authorize a co-bundled `dropColumn` at
//     version W. Scope = {V}. The deploy is REFUSED (ApprovalNotScoped { W }) and
//     NOTHING is applied — the `legacy` column is still present.
//
//     RED PRE-FIX: a blanket `Approval::Approved` (no scope) applied BOTH ops, so the
//     `legacy` column would be dropped. The scope is what fail-closed refuses W.
#[compio::test]
async fn approving_rename_version_does_not_authorize_co_bundled_dropcolumn() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    setup(&conn, &cfg).await;
    let be = PostgresBackend::new(&conn);
    let engine = MigrationEngine::new();
    create_members(&engine, &be, &cfg).await;

    let live = live_members();
    let rename = rename_step(&cfg, "members", "email", "email_address", &live);
    let drop = drop_column_step(&cfg, "members", "legacy", &live);
    let v_rename = scope_version(&rename); // the rename's E1 plan-group version
    let w_drop = scope_version(&drop); // the dropColumn's deterministic version

    // The bundle carries BOTH ops; the operator only reviewed (and scoped) the rename.
    // Order the un-reviewed destructive drop FIRST so the fail-closed scope refusal
    // fires BEFORE any step mutates state — pinning the strict "deploy applies
    // NOTHING" guarantee (the additive rename EXPAND never even runs). The refusal of
    // W is independent of order; this ordering lets us also assert total atomicity.
    let steps = vec![drop, rename];
    let touched: Vec<String> = vec!["members".into()];
    let scope = ApprovalScope::Versions([v_rename.clone()].into_iter().collect());

    let before = journal_count(&conn, &cfg).await;
    let err = engine
        .apply_plan_with_touched_and_depends_scoped(
            &steps,
            &touched,
            &[],
            Approval::Approved,
            &scope,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await
        .expect_err("a co-bundled unreviewed dropColumn must be refused even under approval");

    assert_not_scoped(&err, Some(&w_drop));

    // FAIL CLOSED: nothing applied — `legacy` still present, journal unchanged.
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "legacy").await,
        "the refused deploy must NOT have dropped the un-reviewed `legacy` column"
    );
    assert_eq!(
        journal_count(&conn, &cfg).await,
        before,
        "the refused deploy must have journaled nothing new"
    );

    teardown(&conn, &cfg).await;
}

// (2) Approving the EXACT destructive version authorizes ONLY that op: a lone
//     `dropColumn` at W applies under scope {W}; the same op under scope {V} (some
//     unrelated version) is refused.
#[compio::test]
async fn approving_the_exact_destructive_version_authorizes_only_that_op() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    setup(&conn, &cfg).await;
    let be = PostgresBackend::new(&conn);
    let engine = MigrationEngine::new();
    create_members(&engine, &be, &cfg).await;

    let live = live_members();
    let drop = drop_column_step(&cfg, "members", "legacy", &live);
    let w_drop = scope_version(&drop);

    // Under scope {someOtherVersion} → REFUSED.
    let wrong_scope = ApprovalScope::Versions(["mig_NotTheRealVersion000000".into()].into_iter().collect());
    let err = engine
        .apply_plan_with_touched_and_depends_scoped(
            std::slice::from_ref(&drop),
            &["members".into()],
            &[],
            Approval::Approved,
            &wrong_scope,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await
        .expect_err("the wrong scope must refuse the drop");
    assert_not_scoped(&err, Some(&w_drop));
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "legacy").await,
        "the wrong-scope deploy applied nothing"
    );

    // Under scope {W} → APPLIES (the operator reviewed exactly this op).
    let right_scope = ApprovalScope::Versions([w_drop.clone()].into_iter().collect());
    engine
        .apply_plan_with_touched_and_depends_scoped(
            std::slice::from_ref(&drop),
            &["members".into()],
            &[],
            Approval::Approved,
            &right_scope,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await
        .expect("the exact-version scope must authorize the drop");
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "legacy").await,
        "the in-scope drop must have removed `legacy`"
    );

    teardown(&conn, &cfg).await;
}

// (3) An EMPTY scope under `Approval::Approved` authorizes NO destructive op
//     (fail-closed) — an unrecognized/empty scope authorizes nothing destructive.
#[compio::test]
async fn unscoped_empty_scope_authorizes_no_destructive_op() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    setup(&conn, &cfg).await;
    let be = PostgresBackend::new(&conn);
    let engine = MigrationEngine::new();
    create_members(&engine, &be, &cfg).await;

    let live = live_members();
    let drop = drop_column_step(&cfg, "members", "legacy", &live);
    let empty = ApprovalScope::Versions(std::collections::BTreeSet::new());

    let err = engine
        .apply_plan_with_touched_and_depends_scoped(
            std::slice::from_ref(&drop),
            &["members".into()],
            &[],
            Approval::Approved,
            &empty,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await
        .expect_err("an empty scope must authorize NOTHING destructive");
    assert_not_scoped(&err, None);
    assert!(
        column_exists(&conn, &cfg.project_schema, "members", "legacy").await,
        "the empty-scope deploy applied nothing"
    );

    teardown(&conn, &cfg).await;
}

// (4) `ApprovalScope::All` + `Approval::Approved` preserves the legacy blanket
//     behavior — the destructive op applies (byte-identical to pre-PR9b). Pins the
//     no-regression default.
#[compio::test]
async fn approval_scope_all_preserves_legacy_blanket_behavior() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    setup(&conn, &cfg).await;
    let be = PostgresBackend::new(&conn);
    let engine = MigrationEngine::new();
    create_members(&engine, &be, &cfg).await;

    let live = live_members();
    let drop = drop_column_step(&cfg, "members", "legacy", &live);

    engine
        .apply_plan_with_touched_and_depends_scoped(
            std::slice::from_ref(&drop),
            &["members".into()],
            &[],
            Approval::Approved,
            &ApprovalScope::All,
            &be,
            &cfg,
            "app_test",
            zeroship_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await
        .expect("ApprovalScope::All must apply the destructive op (legacy blanket behavior)");
    assert!(
        !column_exists(&conn, &cfg.project_schema, "members", "legacy").await,
        "ApprovalScope::All must have dropped `legacy` (no-regression)"
    );

    teardown(&conn, &cfg).await;
}
