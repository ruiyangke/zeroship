//! Rolling back a dropped sequence restores it, proven against live `PostgreSQL`.
//!
//! `Op::DropSequence` lowers with no `down`, so a rollback leaves the sequence
//! gone. Unlike a view, nothing new has to be recorded to fix it: the fold already
//! keeps the whole `SequenceSnapshot` - increment, start, bounds, cache, cycle,
//! ownership - so the `CREATE SEQUENCE` that undoes the drop can be rendered from
//! facts the history is already carrying.
//!
//! Sequences are PostgreSQL-only here (`Capability::Sequence`), so there is no
//! SQLite sibling for this one.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`. Assertions read the live catalog
//! through `snapshot_schema`, never the plan the engine intended to run.

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
use zero_migrate::model::snapshot::SequenceSnapshot;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, snapshot_schema, Approval, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_sequence_rollback_pg";
const SEQ: &str = "order_no";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_sequence_rollback_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A sequence with non-default settings throughout, so a restore that silently
/// fell back to PostgreSQL's defaults would not compare equal.
fn create_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_order_no",
        "owner_app": OWNER,
        "ops": [{
            "op": "createSequence",
            "name": SEQ,
            "as": "bigInt",
            "increment": 7,
            "start": 500,
            "minValue": 100,
            "maxValue": 900_000,
            "cache": 12,
            "cycle": true
        }]
    })
    .to_string()
}

fn drop_doc(guarded: bool) -> String {
    let mut op = serde_json::json!({"op": "dropSequence", "name": SEQ});
    if guarded {
        op["existenceGuard"] = serde_json::json!("ifExists");
    }
    serde_json::json!({
        "ir_version": 1,
        "name": if guarded { "drop_order_no_if_present" } else { "drop_order_no" },
        "owner_app": OWNER,
        "ops": [op]
    })
    .to_string()
}

/// Apply one IR doc through the real lower + apply path against live `PostgreSQL`.
///
/// `history` accumulates the applied op stream and is folded into the live schema
/// the way the deploy path does it (`refresh_historical_live`, engine.rs:390-392).
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

/// The sequence as the live catalog reports it, or `None` when absent.
async fn live_sequence(
    session: &PgDevSession,
    schema: &str,
) -> Result<Option<SequenceSnapshot>, String> {
    let snapshot = snapshot_schema(session, schema)
        .await
        .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
    Ok(snapshot.sequences.get(SEQ).cloned())
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn rolling_back_a_dropped_sequence_restores_it() {
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

        let before = live_sequence(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "the sequence must exist before it is dropped".to_string())?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(false),
                &BTreeMap::new(),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if live_sequence(&session, &cfg.project_schema).await?.is_some() {
            return Err("the drop must actually remove the sequence".into());
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
        .map_err(|error| format!("rolling back the dropped sequence must succeed: {error}"))?;

        let after = live_sequence(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "rolling back the drop must put the sequence back".to_string())?;
        if after != before {
            return Err(format!(
                "the restored sequence must carry the settings it had before the drop\n  before: {before:?}\n   after: {after:?}"
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

/// A guarded drop keeps no inverse, for the same reason it does not on views: an
/// `ifExists` drop can journal `completed` without running, so re-creating on
/// rollback would conjure a sequence that never existed here.
#[compio::test]
async fn a_guarded_sequence_drop_keeps_no_inverse() {
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
        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(true),
                &BTreeMap::new(),
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
            pg_guard(&cfg).as_ref(),
        )
        .await
        .err()
        .ok_or_else(|| "a guarded drop must not be reversible".to_string())?;

        if !matches!(error, RollbackError::Irreversible { .. }) {
            return Err(format!(
                "expected the planner to refuse the guarded drop as irreversible, got {error:?}"
            ));
        }
        if live_sequence(&session, &cfg.project_schema)
            .await?
            .is_some()
        {
            return Err("the refused rollback must not have re-created the sequence".into());
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
