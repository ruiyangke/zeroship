//! Refuse rollback after dropping a consumed PostgreSQL sequence.
//!
//! The setup issues values before the drop because a sequence definition does not
//! include its runtime position, and recreating it could issue those values again.

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
    fold_ops, guard_for, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_sequence_position_pg";
const SEQ: &str = "order_no";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_sequence_position_pg_{}_{}",
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
        "name": "create_order_no",
        "owner_app": OWNER,
        "ops": [{
            "op": "createSequence",
            "name": SEQ,
            "as": "bigInt",
            "increment": 1,
            "start": 1,
            "cache": 1
        }]
    })
    .to_string()
}

fn drop_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_order_no",
        "owner_app": OWNER,
        "ops": [{"op": "dropSequence", "name": SEQ}]
    })
    .to_string()
}

async fn apply_doc(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    ir: &str,
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
        &BTreeMap::new(),
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
            PlanStep::Ddl(migration) => Some(migration.clone()),
            _ => None,
        })
        .collect())
}

async fn next_sequence_value(session: &PgDevSession, schema: &str) -> Result<i64, String> {
    session
        .query_one(
            "SELECT nextval(format('%I.%I', $1::text, $2::text)::regclass) AS value",
            &[schema.into(), SEQ.into()],
        )
        .await
        .map_err(|error| format!("read the next live sequence value: {error}"))?
        .try_get("value")
        .map_err(|error| format!("decode the next live sequence value: {error}"))
}

async fn sequence_exists(session: &PgDevSession, schema: &str) -> Result<bool, String> {
    session
        .query_one(
            "SELECT to_regclass(format('%I.%I', $1::text, $2::text)) IS NOT NULL AS present",
            &[schema.into(), SEQ.into()],
        )
        .await
        .map_err(|error| format!("check the live sequence relation: {error}"))?
        .try_get("present")
        .map_err(|error| format!("decode live sequence presence: {error}"))
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn rolling_back_a_consumed_dropped_sequence_is_refused() {
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
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated test schema");

    let work: Result<RollbackError, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history: Vec<Op> = Vec::new();
        let mut migrations =
            apply_doc(&session, &cfg, &create_doc(), &mut history, Approval::None).await?;

        for expected in 1..=5 {
            let issued = next_sequence_value(&session, &cfg.project_schema).await?;
            if issued != expected {
                return Err(format!(
                    "the sequence must issue {expected} before the drop, got {issued}"
                ));
            }
        }

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if sequence_exists(&session, &cfg.project_schema).await? {
            return Err("the drop must actually remove the sequence".into());
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
        .ok_or_else(|| "rolling back the consumed sequence must be refused".to_string())?;

        if sequence_exists(&session, &cfg.project_schema).await? {
            return Err("the refused rollback must not re-create the sequence".into());
        }
        Ok(error)
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    let error = match (work, cleanup) {
        (Ok(error), Ok(())) => error,
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    };

    assert!(
        matches!(&error, RollbackError::Irreversible { .. }),
        "expected the consumed sequence drop to be irreversible, got {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("is irreversible (down: None); rollback refuses by default"),
        "expected the explicit irreversible rollback refusal, got {error}"
    );
}
