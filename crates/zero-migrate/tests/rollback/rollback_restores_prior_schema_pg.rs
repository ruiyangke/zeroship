//! The PostgreSQL half of `rollback_restores_prior_schema.rs`.
//!
//! That file states its own gap: of the ops that render a `down`, it exercises
//! `createTable`, `addColumn`, `createIndex` and `renameTable`, and cannot reach
//! `addConstraint` or `setColumnNotNull` because SQLite REFUSES both
//! (`SqliteRebuildOnly`, `NativeAlterColumn`). Those two are the interesting
//! ones: their reverses drop a constraint and restore a nullability, neither of
//! which is visible in a table list.
//!
//! THE ORACLE IS THE CATALOG, read twice. Column nullability from
//! `information_schema.columns` and constraint definitions from
//! `pg_get_constraintdef`, captured before the apply and compared after the
//! rollback. A rollback that dropped the wrong constraint, or left the column
//! `NOT NULL`, differs here even though the table still exists with the same
//! columns.
//!
//! It also asserts the apply CHANGED the catalog first. Without that, an op that
//! silently did nothing would "restore" trivially and read as a clean pass.
//!
//! GATE: `ZERO_MIGRATE_TEST_PG_URL`.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::driver::SqlSession;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    guard_for, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine,
    PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_rollback_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "rollback_restore_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// Nullability and constraint definitions for the schema, as a sorted list.
async fn catalog_of(session: &PgDevSession, schema: &str) -> Vec<String> {
    // The schema name is a test-generated token, inlined because this session's
    // bind helper takes its own `Bind` type and the value is not user input.
    let columns = session
        .query(
            &format!(
                "SELECT table_name || '.' || column_name || ' nullable=' || is_nullable AS v \
                 FROM information_schema.columns WHERE table_schema = '{schema}' \
                 ORDER BY table_name, column_name"
            ),
            &[],
        )
        .await
        .expect("read live column nullability");
    let constraints = session
        .query(
            &format!(
                "SELECT c.conname || ' := ' || pg_get_constraintdef(c.oid) AS v \
                 FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
                 WHERE n.nspname = '{schema}' ORDER BY c.conname"
            ),
            &[],
        )
        .await
        .expect("read live constraint definitions");
    let mut out: Vec<String> = columns
        .iter()
        .chain(constraints.iter())
        .map(|row| row.try_get::<_, String>("v").expect("decode a catalog row"))
        .collect();
    out.sort();
    out
}

#[compio::test]
async fn an_engine_rendered_down_restores_the_catalog_on_postgres() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // Each op gets its own schema so the two cases cannot disturb each other.
    for (label, op) in [
        (
            "addConstraint",
            r#"{"op":"addConstraint","table":"t1","constraint":{"name":"t1_c1_uq","kind":{"kind":"unique","columns":["c1"]}}}"#,
        ),
        (
            "setColumnNotNull",
            r#"{"op":"setColumnNotNull","table":"t1","column":"c1"}"#,
        ),
    ] {
        let schema = token();
        let cfg = ExecutorConfig::new(
            format!("project_{schema}"),
            &schema,
            support::no_inject(&schema),
        );
        let _guard = support::SchemaGuard::arm(
            &session,
            [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
        );
        session
            .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
            .await
            .expect("create the isolated test schema");

        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .expect("ensure the migration journal");

        let author = IrAuthor::new(
            &cfg.project_schema,
            OWNER,
            SqlDialect::Postgres,
            &support::confined_charter(),
        );
        let guard_cfg = GuardConfig::from_policy(
            support::no_inject(&cfg.project_schema),
            SqlDialect::Postgres,
        );
        let registry: BTreeMap<String, String> = [("t1".to_string(), OWNER.to_string())]
            .into_iter()
            .collect();

        // Seed a table with a NULLABLE column, so both ops have something to act on.
        let seed = r#"{"ir_version":1,"name":"seed","ops":[{"op":"createTable","name":"t1","columns":[{"name":"c0","type":"bigInt","nullable":false},{"name":"c1","type":"bigInt","nullable":true}],"primaryKey":["c0"]}]}"#;
        let seeded = author
            .load_and_lower_guarded(seed, OWNER, &registry, &LiveSchema::default(), &guard_cfg)
            .expect("the seed lowers");
        MigrationEngine::new()
            .apply_plan(
                &seeded.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                "seed",
                LockMode::Acquire,
            )
            .await
            .expect("the seed applies");

        let before = catalog_of(&session, &cfg.project_schema).await;

        let mut live = LiveSchema::default();
        live.tables.insert("t1".into());
        let bytes = format!(r#"{{"ir_version":1,"name":"rb_{label}","ops":[{op}]}}"#);
        let artifact = author
            .load_and_lower_guarded(&bytes, OWNER, &registry, &live, &guard_cfg)
            .unwrap_or_else(|e| panic!("{label}: must lower on PostgreSQL: {e:?}"));

        let migrations: Vec<_> = artifact
            .plan
            .steps
            .iter()
            .filter_map(|step| match step {
                PlanStep::Ddl(migration) => Some(migration.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !migrations.is_empty(),
            "{label}: rendered no DDL to roll back"
        );

        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                "forward",
                LockMode::Acquire,
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: the forward apply must succeed: {e:?}"));

        let after_apply = catalog_of(&session, &cfg.project_schema).await;
        assert_ne!(
            before, after_apply,
            "{label}: the apply changed nothing in the catalog, so rolling it back \
             proves nothing"
        );

        let request = RollbackRequest::new(RollbackTarget::Steps(migrations.len()));
        rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            guard_for(&guard_cfg).as_ref(),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: the rollback must succeed: {e}"));

        let after_rollback = catalog_of(&session, &cfg.project_schema).await;
        assert_eq!(
            before, after_rollback,
            "{label}: the engine rendered a down that does NOT restore the catalog. \
             Compared over column nullability and constraint definitions, so a dropped \
             wrong constraint or a column left NOT NULL shows up here even though the \
             table still exists with the same columns"
        );
    }
}
