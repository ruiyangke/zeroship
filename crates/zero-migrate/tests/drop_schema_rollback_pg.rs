//! Rolling back a dropped schema restores it, EXCEPT when the drop cascaded.
//!
//! `Op::DropSchema` carries `cascade`, and that flag decides whether an inverse is
//! honest. Without it the drop is RESTRICT, which PostgreSQL only permits on an
//! empty schema, so `CREATE SCHEMA` puts back exactly what was removed. With it,
//! the drop destroys every table, view and sequence inside, and a synthesised
//! `CREATE SCHEMA` would report a successful rollback over data that is gone -
//! measured, not assumed:
//!
//!     DROP SCHEMA s CASCADE;   -- NOTICE: drop cascades to table s.keepme
//!     CREATE SCHEMA s;         -- CREATE SCHEMA
//!     SELECT count(*) FROM pg_tables WHERE schemaname='s';   -- 0
//!
//! So a cascading drop keeps `down: None` and the planner refuses it. That is the
//! third refusal in this family, on top of the guarded-drop and no-history ones.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`. Existence is read from the live
//! catalog through `snapshot_schema`, which queries `pg_namespace` by name.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{
    rollback, LockMode, RollbackError, RollbackRequest, RollbackTarget,
};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, snapshot_schema, Approval, EffectivePolicy, ExecutorConfig, GuardConfig,
    IrAuthor, LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_schema_rollback_pg";

fn token(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_schema_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Vendor ops need the operator/platform capability set; a creator profile refuses
/// them at load with "the privileged zero-migrate primitives are unreachable from a
/// confined migration by construction".
fn policy(schema: &str) -> EffectivePolicy {
    support::operator_charter(schema)
}

fn create_doc(target: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": format!("create_schema_{target}"),
        "owner_app": OWNER,
        "ops": [{"op": "createSchema", "name": target}]
    })
    .to_string()
}

fn drop_doc(target: &str, cascade: bool) -> String {
    let mut op = serde_json::json!({"op": "dropSchema", "name": target});
    if cascade {
        op["cascade"] = serde_json::json!(true);
    }
    serde_json::json!({
        "ir_version": 1,
        "name": if cascade { format!("drop_schema_{target}_cascade") } else { format!("drop_schema_{target}") },
        "owner_app": OWNER,
        "ops": [op]
    })
    .to_string()
}

/// Apply one IR doc through the guarded deploy entry, which is what carries the
/// author's `VendorAuthority`; a bare `load_ir_document` refuses every privileged
/// primitive.
async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    history: &mut Vec<Op>,
    approval: Approval,
) -> Result<Vec<Migration>, String> {
    let backend = PostgresBackend::new_generic(session);
    let pol = policy(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &pol);
    let guard = GuardConfig::from_policy(pol.clone(), SqlDialect::Postgres);
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &pol)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let artifact = author
        .load_and_lower_guarded(ir, OWNER, &BTreeMap::new(), &live, &guard)
        .map_err(|error| format!("load and lower the guarded plan: {error}"))?;
    let authored: zero_migrate::MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse the authored IR: {error}"))?;
    history.extend(authored.ops);
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            approval,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    Ok(artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect())
}

/// Does the schema PHYSICALLY exist? `snapshot_schema` looks it up in
/// `pg_namespace` by name, so an empty `schemas` map means absent.
async fn schema_exists(session: &PgDevSession, name: &str) -> Result<bool, String> {
    let snapshot = snapshot_schema(session, name)
        .await
        .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
    Ok(snapshot.schemas.contains_key(name))
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        policy(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn rolling_back_a_dropped_schema_restores_it() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("home");
    let target = token("target");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    let quoted_target = quote_ident(&target);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut migrations = apply_doc(
            &session,
            &cfg,
            &create_doc(&target),
            &mut history,
            Approval::None,
        )
        .await?;
        if !schema_exists(&session, &target).await? {
            return Err("the schema must exist before it is dropped".into());
        }

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(&target, false),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if schema_exists(&session, &target).await? {
            return Err("the drop must actually remove the schema".into());
        }

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            pg_guard(&cfg).as_ref(),
        )
        .await
        .map_err(|error| format!("rolling back the dropped schema must succeed: {error}"))?;

        if !schema_exists(&session, &target).await? {
            return Err("rolling back the drop must put the schema back".into());
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_target} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

/// A CASCADING drop is never reversed, and the table it destroyed proves why.
///
/// `CREATE SCHEMA` would succeed here and restore an EMPTY schema while the table
/// stays gone, so the rollback would journal a clean success over destroyed data.
/// The planner must refuse instead.
#[compio::test]
async fn a_cascading_schema_drop_keeps_no_inverse() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token("home");
    let target = token("cascade");
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    let quoted_target = quote_ident(&target);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut migrations = apply_doc(
            &session,
            &cfg,
            &create_doc(&target),
            &mut history,
            Approval::None,
        )
        .await?;

        // Put something in the schema, out of band, so the cascade has real
        // contents to destroy. Authoring it through the migration would fight the
        // schema confinement this test does not mean to exercise.
        session
            .batch(&format!(
                "CREATE TABLE {quoted_target}.keepme (id int primary key)"
            ))
            .await
            .map_err(|error| format!("seed a table inside the target schema: {error}"))?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(&target, true),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if schema_exists(&session, &target).await? {
            return Err("the cascading drop must actually remove the schema".into());
        }

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        let error = rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            pg_guard(&cfg).as_ref(),
        )
        .await
        .err()
        .ok_or_else(|| {
            "a cascading drop must not be reversible - the contents are gone".to_string()
        })?;

        if !matches!(error, RollbackError::Irreversible { .. }) {
            return Err(format!(
                "expected the planner to refuse the cascading drop as irreversible, got {error:?}"
            ));
        }
        if schema_exists(&session, &target).await? {
            return Err("the refused rollback must not have re-created the schema".into());
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_target} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}
