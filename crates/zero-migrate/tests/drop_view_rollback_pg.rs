//! Rolling back a dropped view restores it, proven against live `PostgreSQL`.
//!
//! The `SQLite` sibling (`drop_view_rollback_sqlite.rs`) proves the same property
//! on an embedded file. This one proves it on a real server, because the two
//! backends reach views through different code: `PostgreSQL` qualifies the view by
//! schema, runs the drop under the project advisory lock, and introspects the body
//! back through `pg_get_viewdef`. A rendering that worked only on `SQLite` would
//! pass there and fail here.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`, like every other live suite here, and
//! it announces the skip rather than reporting the same count either way.
//!
//! Every assertion reads the live catalog through `snapshot_schema`, never the plan
//! the engine intended to run.

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
    fold_ops, guard_for, snapshot_schema, Approval, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_view_rollback_pg";
const VIEW: &str = "active_users";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_view_rollback_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn create_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_active_users",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "users",
                "columns": [{"name": "email", "type": "text", "nullable": false}]
            },
            {
                "op": "createView",
                "name": VIEW,
                "query": {
                    "kind": "structured",
                    "select": {
                        "from": {"name": "users"},
                        "projection": [{"kind": "colRef", "name": "email"}],
                        "joins": [],
                        "groupBy": []
                    }
                }
            }
        ]
    })
    .to_string()
}

fn drop_doc(guarded: bool) -> String {
    let mut op = serde_json::json!({"op": "dropView", "name": VIEW});
    if guarded {
        op["existenceGuard"] = serde_json::json!("ifExists");
    }
    serde_json::json!({
        "ir_version": 1,
        "name": if guarded { "drop_active_users_if_present" } else { "drop_active_users" },
        "owner_app": OWNER,
        "ops": [op]
    })
    .to_string()
}

/// Apply one IR doc through the real lower + apply path against live `PostgreSQL`.
///
/// `history` accumulates the applied op stream and is folded into the live schema
/// the way the deploy path does it (`refresh_historical_live`, engine.rs:390-392).
/// That fold is what carries the earlier `createView` body forward to the
/// `dropView` that undoes it.
async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &BTreeMap<String, String>,
    history: &mut Vec<Op>,
    approval: Approval,
) -> Result<Vec<Migration>, String> {
    let backend = PostgresBackend::new_generic(session);
    let policy = support::no_inject(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
    let document = zero_migrate::model::load::load_ir_document(
        ir,
        OWNER,
        zero_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
    )
    .map_err(|error| format!("load gate (postgres): {error}"))?;
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &policy)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    history.extend(document.ops.iter().cloned());
    let plan = author
        .lower_plan(&document, &live)
        .map_err(|error| format!("lower the doc plan on PostgreSQL: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            approval,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply the authored plan on PostgreSQL: {error}"))?;
    Ok(plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect())
}

/// The view's live body as `pg_get_viewdef` reports it, or `None` when absent.
async fn live_view_body(session: &PgDevSession, schema: &str) -> Result<Option<String>, String> {
    let snapshot = snapshot_schema(session, schema)
        .await
        .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
    Ok(snapshot
        .views
        .get(VIEW)
        .map(|view| view.definition.clone().unwrap_or_default()))
}

#[compio::test]
async fn rolling_back_a_dropped_view_restores_it_on_postgres() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
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
            &create_doc(),
            &BTreeMap::new(),
            &mut history,
            Approval::None,
        )
        .await?;

        let before = live_view_body(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "the view must exist before it is dropped".to_string())?;
        if before.is_empty() {
            return Err("pg_get_viewdef must report a body for the restore to be comparable".into());
        }

        let registry: BTreeMap<String, String> = [("users".to_string(), OWNER.to_string())]
            .into_iter()
            .collect();
        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(false),
                &registry,
                &mut history,
                Approval::Approved,
            )
            .await?,
        );

        if live_view_body(&session, &cfg.project_schema).await?.is_some() {
            return Err("the drop must actually remove the view".into());
        }

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            guard_for(&GuardConfig::from_policy(
                support::no_inject(&cfg.project_schema),
                SqlDialect::Postgres,
            ))
            .as_ref(),
        )
        .await
        .map_err(|error| format!("rolling back the dropped view must succeed: {error}"))?;

        let after = live_view_body(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "rolling back the drop must put the view back".to_string())?;
        if after != before {
            return Err(format!(
                "the restored view body must match the one that was dropped\n  before: {before}\n   after: {after}"
            ));
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
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

/// A guarded drop keeps no inverse on `PostgreSQL` either.
///
/// `ifExists` can journal `completed` without running the `DROP`, so re-creating
/// on rollback would conjure a view that never existed on this database.
#[compio::test]
async fn a_guarded_drop_keeps_no_inverse_on_postgres() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
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
            &create_doc(),
            &BTreeMap::new(),
            &mut history,
            Approval::None,
        )
        .await?;

        let registry: BTreeMap<String, String> = [("users".to_string(), OWNER.to_string())]
            .into_iter()
            .collect();
        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(true),
                &registry,
                &mut history,
                Approval::Approved,
            )
            .await?,
        );

        let request = RollbackRequest::new(RollbackTarget::Steps(1));
        let error = rollback(
            &backend,
            &cfg,
            &request,
            &migrations,
            Approval::Approved,
            OWNER,
            guard_for(&GuardConfig::from_policy(
                support::no_inject(&cfg.project_schema),
                SqlDialect::Postgres,
            ))
            .as_ref(),
        )
        .await
        .err()
        .ok_or_else(|| "a guarded drop must not be reversible".to_string())?;

        if !matches!(error, RollbackError::Irreversible { .. }) {
            return Err(format!(
                "expected the planner to refuse the guarded drop as irreversible, got {error:?}"
            ));
        }
        if live_view_body(&session, &cfg.project_schema)
            .await?
            .is_some()
        {
            return Err("the refused rollback must not have re-created the view".into());
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
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

/// A Rust host embedding the engine directly gets NO pending-schema projection, so a
/// `dropView` naming a view nothing created reaches PostgreSQL and PostgreSQL refuses it.
///
/// The Node addon refuses the same authored migration one layer earlier, with
/// `failed to project pending schema after envelope ...`, because
/// `lower_ordered_envelopes_to_plans_for_apply` folds pending ops onto the catalog
/// snapshot first. `ProjectionGuardVerdict` exists in exactly one file of one crate -
/// `crates/zero-migrate-node/src/lower.rs` - so nothing on this path can produce that
/// refusal.
///
/// This arm exists to make the difference concrete rather than argued. What it shows is
/// WHERE the refusal comes from, not that one host is safer: both refuse, and the
/// authored migration is rejected either way. A claim that a Rust embedder is less
/// protected needs an op that the projection refuses and the database ACCEPTS, which
/// this is not and which nobody has produced yet.
#[compio::test]
async fn a_rust_embedding_refuses_an_absent_view_drop_at_the_database() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let quoted_schema = quote_ident(&cfg.project_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure the journal: {error}"))?;

        let reg = BTreeMap::new();
        let mut history: Vec<Op> = Vec::new();
        // No createView anywhere in this history, and no prior migration at all.
        let outcome = apply_doc(
            &session,
            &cfg,
            &drop_doc(false),
            &reg,
            &mut history,
            Approval::Approved,
        )
        .await;

        let error = outcome.expect_err("dropping a view nothing created must not succeed");
        assert!(
            !error.contains("failed to project pending schema"),
            "a Rust embedding builds no projection, so the refusal cannot come from it: {error}"
        );
        assert!(
            error.contains(VIEW),
            "the refusal names the view PostgreSQL could not find: {error}"
        );
        Ok(())
    }
    .await;

    work.expect("the Rust embedding refuses an absent view drop");
}
