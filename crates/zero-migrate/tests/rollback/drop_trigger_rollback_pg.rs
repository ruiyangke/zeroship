//! Trigger-drop rollback lowering from folded migration history.
//!
//! PostgreSQL identifies a trigger by schema, table, and name. These tests lower
//! through the guarded authoring path with a `LiveSchema` built from `fold_ops`,
//! matching the deploy path that refreshes historical live state. The live
//! rollback case is gated behind `ZERO_MIGRATE_TEST_PG_URL`; the focused controls
//! always run.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::expr::{Expr, UnaryOp};
use zero_migrate::model::ir::{ForEach, Op, TriggerAction, TriggerEvent, TriggerTiming};
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_trigger_rollback_pg";
const PROJECT_SCHEMA: &str = "zero_migrate";
const RECORDED_SCHEMA: &str = "recorded_trigger_schema";
const TRIGGER: &str = "audit";
const ORDERS: &str = "orders";
const INVOICES: &str = "invoices";
const LIVE_TABLE: &str = "events";
const LIVE_TRIGGER: &str = "audit_payload";
const LIVE_FUNCTION: &str = "audit_payload_fn";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_trigger_rollback_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn policy(schema: &str) -> EffectivePolicy {
    support::operator_charter(schema)
}

fn when_payload_is_not_null() -> Expr {
    Expr::UnaryOp {
        op: UnaryOp::IsNotNull,
        operand: Box::new(Expr::ColRef {
            name: "payload".to_string(),
            table: Some("new".to_string()),
        }),
    }
}

fn orders_trigger_op() -> Op {
    Op::CreateTrigger {
        name: TRIGGER.to_string(),
        table: ORDERS.to_string(),
        schema: Some(RECORDED_SCHEMA.to_string()),
        timing: TriggerTiming::After,
        events: vec![TriggerEvent::Insert, TriggerEvent::Update],
        for_each: ForEach::Row,
        action: TriggerAction::ExecuteFunction {
            name: "audit_orders".to_string(),
        },
        when: Some(when_payload_is_not_null()),
    }
}

fn invoices_trigger_op() -> Op {
    Op::CreateTrigger {
        name: TRIGGER.to_string(),
        table: INVOICES.to_string(),
        schema: Some(RECORDED_SCHEMA.to_string()),
        timing: TriggerTiming::Before,
        events: vec![TriggerEvent::Truncate],
        for_each: ForEach::Statement,
        action: TriggerAction::ExecuteFunction {
            name: "audit_invoices".to_string(),
        },
        when: None,
    }
}

fn drop_trigger_op(table: &str, if_exists: Option<bool>) -> Op {
    Op::DropTrigger {
        name: TRIGGER.to_string(),
        table: table.to_string(),
        schema: Some(RECORDED_SCHEMA.to_string()),
        if_exists,
    }
}

fn registry(tables: &[&str]) -> BTreeMap<String, String> {
    tables
        .iter()
        .map(|table| ((*table).to_string(), OWNER.to_string()))
        .collect()
}

fn lower_drop_from_history(history: &[Op], table: &str, if_exists: Option<bool>) -> Migration {
    let dialect = SqlDialect::Postgres;
    let pol = policy(PROJECT_SCHEMA);
    let folded =
        fold_ops(history, dialect, PROJECT_SCHEMA, &pol).expect("the trigger history must fold");
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let mut drop = serde_json::json!({
        "op": "dropTrigger",
        "name": TRIGGER,
        "table": table,
        "schema": RECORDED_SCHEMA,
    });
    if let Some(if_exists) = if_exists {
        drop["ifExists"] = serde_json::json!(if_exists);
    }
    let document = serde_json::json!({
        "ir_version": 1,
        "name": format!("drop_{table}_{TRIGGER}"),
        "owner_app": OWNER,
        "ops": [drop],
    })
    .to_string();
    let guard = GuardConfig::from_policy(pol.clone(), dialect);
    let artifact = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &pol)
        .load_and_lower_guarded(
            &document,
            OWNER,
            &registry(&[ORDERS, INVOICES]),
            &live,
            &guard,
        )
        .expect("the trigger drop must lower");
    let [PlanStep::Ddl(migration)] = artifact.plan.steps.as_slice() else {
        panic!("expected one trigger DDL step")
    };
    migration.clone()
}

fn orders_inverse() -> &'static str {
    "CREATE TRIGGER \"audit\" AFTER INSERT OR UPDATE ON \
     \"recorded_trigger_schema\".\"orders\" FOR EACH ROW \
     WHEN ((\"new\".\"payload\" IS NOT NULL)) EXECUTE FUNCTION \
     \"recorded_trigger_schema\".\"audit_orders\"()"
}

fn invoices_inverse() -> &'static str {
    "CREATE TRIGGER \"audit\" BEFORE TRUNCATE ON \
     \"recorded_trigger_schema\".\"invoices\" FOR EACH STATEMENT EXECUTE FUNCTION \
     \"recorded_trigger_schema\".\"audit_invoices\"()"
}

fn create_live_trigger_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_audit_payload",
        "owner_app": OWNER,
        "ops": [{
            "op": "createTrigger",
            "name": LIVE_TRIGGER,
            "table": LIVE_TABLE,
            "schema": schema,
            "timing": "after",
            "events": ["insert", "update"],
            "forEach": "row",
            "action": { "kind": "executeFunction", "name": LIVE_FUNCTION },
            "when": {
                "node": "unaryOp",
                "op": "isNotNull",
                "operand": { "node": "colRef", "name": "payload", "table": "new" }
            }
        }],
    })
    .to_string()
}

fn drop_live_trigger_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_audit_payload",
        "owner_app": OWNER,
        "ops": [{
            "op": "dropTrigger",
            "name": LIVE_TRIGGER,
            "table": LIVE_TABLE,
            "schema": schema,
        }],
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
    let pol = policy(&cfg.project_schema);
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &pol);
    let guard = GuardConfig::from_policy(pol.clone(), SqlDialect::Postgres);
    let folded = fold_ops(history, SqlDialect::Postgres, &cfg.project_schema, &pol)
        .map_err(|error| format!("fold the applied history: {error}"))?;
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let artifact = author
        .load_and_lower_guarded(ir, OWNER, &registry(&[LIVE_TABLE]), &live, &guard)
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
            PlanStep::Ddl(migration) => Some(migration.clone()),
            _ => None,
        })
        .collect())
}

async fn live_trigger_definition(
    session: &PgDevSession,
    schema: &str,
) -> Result<Option<String>, String> {
    let rows = session
        .query(
            "SELECT pg_get_triggerdef(trigger.oid, true) AS definition \
             FROM pg_catalog.pg_trigger trigger \
             JOIN pg_catalog.pg_class target ON target.oid = trigger.tgrelid \
             JOIN pg_catalog.pg_namespace namespace ON namespace.oid = target.relnamespace \
             WHERE namespace.nspname = $1 AND target.relname = $2 \
               AND trigger.tgname = $3 AND NOT trigger.tgisinternal",
            &[schema.into(), LIVE_TABLE.into(), LIVE_TRIGGER.into()],
        )
        .await
        .map_err(|error| format!("read the trigger from pg_trigger: {error}"))?;
    Ok(rows
        .first()
        .and_then(|row| row.try_get::<_, String>("definition").ok()))
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        policy(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn positive_unguarded_drop_trigger_from_folded_history_has_create_inverse() {
    let migration = lower_drop_from_history(&[orders_trigger_op()], ORDERS, None);

    assert_eq!(migration.down.as_deref(), Some(orders_inverse()));
    guard_for(&GuardConfig::from_policy(
        policy(RECORDED_SCHEMA),
        SqlDialect::Postgres,
    ))
    .as_ref()
    .check(migration.down.as_deref().expect("the inverse exists"))
    .expect("the synthesised inverse must pass the configured guard");
}

#[compio::test]
async fn negative_guarded_drop_trigger_from_folded_history_has_no_inverse() {
    let migration = lower_drop_from_history(&[orders_trigger_op()], ORDERS, Some(true));

    assert_eq!(
        migration.up,
        r#"DROP TRIGGER IF EXISTS "audit" ON "recorded_trigger_schema"."orders""#
    );
    assert_eq!(migration.down, None);
}

#[compio::test]
async fn negative_unguarded_drop_trigger_absent_from_history_has_no_inverse() {
    let migration = lower_drop_from_history(&[], ORDERS, None);

    assert_eq!(migration.down, None);
}

#[compio::test]
async fn positive_explicit_false_drop_trigger_is_eligible_for_an_inverse() {
    let migration = lower_drop_from_history(&[orders_trigger_op()], ORDERS, Some(false));

    assert_eq!(
        migration.up,
        r#"DROP TRIGGER "audit" ON "recorded_trigger_schema"."orders""#
    );
    assert_eq!(migration.down.as_deref(), Some(orders_inverse()));
}

#[compio::test]
async fn positive_same_named_triggers_on_two_tables_restore_only_the_dropped_one() {
    let orders = orders_trigger_op();
    let invoices = invoices_trigger_op();
    let both = vec![orders.clone(), invoices.clone()];

    let folded_both = fold_ops(
        &both,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("both same-named triggers must fold");
    assert_eq!(
        folded_both.triggers.len(),
        2,
        "the snapshot key must retain same-named triggers on different tables"
    );

    let orders_drop = lower_drop_from_history(&both, ORDERS, None);
    assert_eq!(orders_drop.down.as_deref(), Some(orders_inverse()));
    assert!(
        !orders_drop
            .down
            .as_deref()
            .expect("the orders trigger has an inverse")
            .contains("audit_invoices"),
        "the invoices trigger must not overwrite the orders trigger"
    );

    let history_after_drop = vec![orders, invoices, drop_trigger_op(ORDERS, None)];
    let folded_after_drop = fold_ops(
        &history_after_drop,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("dropping one same-named trigger must fold");
    assert_eq!(folded_after_drop.triggers.len(), 1);
    assert_eq!(
        lower_drop_from_history(&history_after_drop, ORDERS, None).down,
        None,
        "the dropped orders trigger must be absent from the folded history"
    );
    assert_eq!(
        lower_drop_from_history(&history_after_drop, INVOICES, None)
            .down
            .as_deref(),
        Some(invoices_inverse()),
        "dropping the orders trigger must retain the same-named invoices trigger"
    );
}

#[compio::test]
async fn positive_rolling_back_a_dropped_trigger_restores_its_definition() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let quoted_table = quote_ident(LIVE_TABLE);
    let quoted_function = quote_ident(LIVE_FUNCTION);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {quoted_schema}; \
             CREATE TABLE {quoted_schema}.{quoted_table} \
                 (id integer PRIMARY KEY, payload text); \
             CREATE FUNCTION {quoted_schema}.{quoted_function}() RETURNS trigger \
                 LANGUAGE plpgsql AS $zstrigger$ BEGIN RETURN NEW; END $zstrigger$"
        ))
        .await
        .expect("create isolated trigger test objects");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let mut history = Vec::new();
        let mut migrations = apply_doc(
            &session,
            &cfg,
            &create_live_trigger_doc(&cfg.project_schema),
            &mut history,
            Approval::None,
        )
        .await?;
        let before = live_trigger_definition(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "the trigger must exist before it is dropped".to_string())?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_live_trigger_doc(&cfg.project_schema),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if live_trigger_definition(&session, &cfg.project_schema)
            .await?
            .is_some()
        {
            return Err("the drop must remove the trigger".to_string());
        }

        rollback(
            &backend,
            &cfg,
            &RollbackRequest::new(RollbackTarget::Steps(1)),
            &migrations,
            Approval::Approved,
            OWNER,
            pg_guard(&cfg).as_ref(),
        )
        .await
        .map_err(|error| format!("rolling back the trigger drop must succeed: {error}"))?;

        let after = live_trigger_definition(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "rollback must restore the trigger".to_string())?;
        if after != before {
            return Err(format!(
                "rollback restored the wrong trigger definition\n  before: {before:?}\n   after: {after:?}"
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
