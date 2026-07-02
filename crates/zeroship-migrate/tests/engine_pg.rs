//! Faithful `MigrationEngine` pipeline tests against a REAL Postgres (no shims).
//!
//! Exercises the full public API end-to-end: author -> plan -> gate(approval) ->
//! `apply::executor::apply`, with the least-privilege migrator role provisioned and the
//! apply running under it (the Plan-3 path). The engine gate is asserted to be
//! ADDITIONAL to the executor's own guard + role (defense in depth): a denied
//! plan applies nothing, a destructive plan needs approval, and a real table
//! lands + is journaled when the gate passes.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE` (the runbook uses `postgres`). Set
//! `MIGRATE_TEST_DB` to override the DSN. Each test uses uniquely-suffixed
//! schemas + project id (→ a unique role), so tests are isolated + re-runnable.

use compio_postgres::Client;
use zeroship_migrate::{
    PostgresBackend,
    plan::author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor},
    provision_migrator, apply::role::deprovision_migrator, Approval, EngineError, ExecutorConfig,
    GuardConfig, Migration, MigrationEngine, MigrationId, migrator_role_name,
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

/// A config + matching migrator role for an isolated project. The schema names
/// double as the project schema the deterministic/raw authors qualify into.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::confined(cfg.project_schema.clone())
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

/// Stand up the project schema + provision the migrator role (the Plan-3 path).
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
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

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column existence");
    !rows.is_empty()
}

/// Count completed journal rows for a version.
async fn journaled(conn: &Client, cfg: &ExecutorConfig, version: &str) -> bool {
    let rows = conn
        .query(
            &format!(
                "SELECT 1 FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed'",
                cfg.pg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("query journal");
    !rows.is_empty()
}

fn det(cfg: &ExecutorConfig) -> DeterministicAuthor {
    DeterministicAuthor::new(cfg.project_schema.clone(), "app_test")
}

fn raw(cfg: &ExecutorConfig) -> RawSqlAuthor {
    RawSqlAuthor::new(cfg.project_schema.clone(), "app_test")
}

// ---------------------------------------------------------------------------
// Gate behavior
// ---------------------------------------------------------------------------

#[compio::test]
async fn denied_plan_errors_and_applies_nothing() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // A COPY … TO PROGRAM (shell RCE) is hard-denied by the guard.
    let evil = raw(&cfg)
        .wrap(
            "rce",
            &format!("COPY \"{}\".\"t\" TO PROGRAM 'sh -c id'", cfg.project_schema),
            None,
        )
        .unwrap();
    let engine = MigrationEngine::new();
    let plan = engine.plan(&[evil], &guard_cfg(&cfg));
    assert!(!plan.is_appliable());

    let err = engine
        .apply(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect_err("denied plan must error even WITH approval");
    assert!(matches!(err, EngineError::Denied(ref d) if d.len() == 1), "got {err:?}");

    // Nothing applied: the journal schema may exist but holds no completed rows,
    // and no project table was created.
    assert!(!table_exists(&conn, &cfg.project_schema, "t").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn destructive_plan_without_approval_errors_and_applies_nothing() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // First create a table so the DROP has a real target, applied cleanly.
    let create = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "legacy".into(),
            columns: vec![Column { name: "id".into(), ty: "bigint".into(), nullable: false }],
        })
        .unwrap();
    let engine = MigrationEngine::new();
    let create_plan = engine.plan(&create, &guard_cfg(&cfg));
    engine
        .apply(&create_plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("safe additive create applies without approval");
    assert!(table_exists(&conn, &cfg.project_schema, "legacy").await);

    // Now a destructive DROP, WITHOUT approval → ApprovalRequired, table stays.
    let drop = raw(&cfg)
        .wrap(
            "drop_legacy",
            &format!("DROP TABLE \"{}\".\"legacy\"", cfg.project_schema),
            None,
        )
        .unwrap();
    let drop_plan = engine.plan(std::slice::from_ref(&drop), &guard_cfg(&cfg));
    assert!(drop_plan.requires_approval);
    let err = engine
        .apply(&drop_plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect_err("destructive plan needs approval");
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    assert!(
        table_exists(&conn, &cfg.project_schema, "legacy").await,
        "table must still exist — nothing applied"
    );

    // With approval, the DROP applies.
    engine
        .apply(&drop_plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("approved destructive plan applies");
    assert!(
        !table_exists(&conn, &cfg.project_schema, "legacy").await,
        "table dropped after approval"
    );
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn safe_additive_plan_applies_without_approval() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let create = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "widgets".into(),
            columns: vec![
                Column { name: "id".into(), ty: "bigint".into(), nullable: false },
                Column { name: "label".into(), ty: "text".into(), nullable: true },
            ],
        })
        .unwrap();
    let version = create[0].version.as_str().to_string();
    let engine = MigrationEngine::new();
    let plan = engine.plan(&create, &guard_cfg(&cfg));
    assert!(!plan.requires_approval);

    let outcome = engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("safe additive applies with no approval");
    assert_eq!(outcome.applied, vec![version.clone()]);
    assert!(table_exists(&conn, &cfg.project_schema, "widgets").await);
    assert!(journaled(&conn, &cfg, &version).await);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// End-to-end author → plan → gate → apply (under the migrator role)
// ---------------------------------------------------------------------------

#[compio::test]
async fn deterministic_create_table_e2e_under_migrator_role() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let migs = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "orders".into(),
            columns: vec![
                Column { name: "id".into(), ty: "bigint".into(), nullable: false },
                Column { name: "total".into(), ty: "numeric".into(), nullable: true },
            ],
        })
        .unwrap();
    let version = migs[0].version.as_str().to_string();

    let engine = MigrationEngine::new();
    let plan = engine.plan(&migs, &guard_cfg(&cfg));
    assert!(plan.is_appliable() && !plan.requires_approval);

    engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("apply");

    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    assert!(journaled(&conn, &cfg, &version).await);

    // The table is owned by the migrator role (apply ran under it) — proves the
    // Plan-3 least-priv path executed the DDL, not the admin.
    let owner: String = conn
        .query_one(
            "SELECT tableowner FROM pg_tables WHERE schemaname = $1 AND tablename = 'orders'",
            &[&cfg.project_schema],
        )
        .await
        .expect("owner query")
        .get("tableowner");
    assert_eq!(owner, cfg.pg.migrator_role.clone().unwrap(), "DDL ran under migrator role");
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn raw_sql_complex_up_gated_then_applied_e2e() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let engine = MigrationEngine::new();

    // Step 1 (deterministic): a base table.
    let base = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "accounts".into(),
            columns: vec![Column { name: "id".into(), ty: "bigint".into(), nullable: false }],
        })
        .unwrap();
    engine
        .apply(&engine.plan(&base, &guard_cfg(&cfg)), Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("base table applies");

    // Step 2 (AI-authored, fed via RawSqlAuthor): a multi-statement expand step —
    // add a nullable column then backfill it. The UPDATE is row-mutating DML, so
    // M4's whole-tree DML classifier gates it even though the DDL half is additive.
    let complex = raw(&cfg)
        .wrap(
            "expand_email",
            &format!(
                "ALTER TABLE \"{s}\".\"accounts\" ADD COLUMN \"email\" text; \
                 UPDATE \"{s}\".\"accounts\" SET \"email\" = 'unknown' WHERE \"email\" IS NULL;",
                s = cfg.project_schema
            ),
            Some(&format!("ALTER TABLE \"{}\".\"accounts\" DROP COLUMN \"email\"", cfg.project_schema)),
        )
        .unwrap();
    let version = complex.version.as_str().to_string();
    let plan = engine.plan(&[complex], &guard_cfg(&cfg));
    assert!(plan.is_appliable());
    assert!(plan.destructive, "raw UPDATE backfill is destructive DML");
    assert!(plan.requires_approval, "raw UPDATE backfill requires approval");

    let err = engine
        .apply(&plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect_err("raw UPDATE backfill must be refused without approval");
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    assert!(
        !column_exists(&conn, &cfg.project_schema, "accounts", "email").await,
        "unapproved gated plan must apply nothing"
    );

    engine
        .apply(&plan, Approval::Approved, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect("approved complex up applies");

    assert!(column_exists(&conn, &cfg.project_schema, "accounts", "email").await);
    assert!(journaled(&conn, &cfg, &version).await);
    teardown(&conn, &cfg).await;
}

/// Defense in depth: even if a caller hand-builds a plan whose `denied` list is
/// empty (bypassing the engine's `plan()` guard), the executor independently
/// re-runs the guard and denies the dangerous `up`. The gate is additional, not
/// the sole check.
#[compio::test]
async fn executor_reruns_guard_even_on_a_hand_built_clean_plan() {
    use zeroship_migrate::{MigrationPlan, PlannedMigration};
    use zeroship_migrate::GuardOutcome;

    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // A dangerous migration with a FAKED-clean plan: empty denied, not
    // destructive, no approval — as if plan() had waved it through.
    let evil = { let mut __mig = Migration {
        version: MigrationId::generate(),
        name: "smuggled".into(),
        up: "SELECT * FROM control.users".into(),
        down: None,
        checksum: zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput { up: "", down: None, flags: &zeroship_migrate::MigrationFlags::default(), owner_app: "", depends_on: &[], supersedes: &[], preconditions: &[] }),
        flags: zeroship_migrate::MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }; __mig.recompute_checksum(); __mig };
    let version = evil.version.as_str().to_string();
    let forged_plan = MigrationPlan {
        items: vec![PlannedMigration {
            migration: evil,
            // A plausible-looking clean outcome.
            report: GuardOutcome {
                destructive: false,
                advisories: Vec::new(),
            },
        }],
        destructive: false,
        requires_approval: false,
        denied: Vec::new(),
    };

    let err = MigrationEngine::new()
        .apply(&forged_plan, Approval::None, &PostgresBackend::new(&conn), &cfg, "app_test")
        .await
        .expect_err("executor's own guard must still deny the cross-tenant read");
    // It comes back as an Apply error (the executor's guard gate), NOT a clean apply.
    assert!(matches!(err, EngineError::Apply(_)), "got {err:?}");
    assert!(!journaled(&conn, &cfg, &version).await, "nothing journaled");
    teardown(&conn, &cfg).await;
}
