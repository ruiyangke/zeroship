//! Policy-drop rollback lowering from folded migration history.
//!
//! PostgreSQL identifies a policy by schema, table, and name. These tests lower
//! through the guarded authoring path with a `LiveSchema` built from `fold_ops`,
//! matching the deploy path that refreshes historical live state. The live
//! rollback case is gated behind `ZERO_MIGRATE_TEST_PG_URL`; the focused controls
//! always run.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::expr::Expr;
use zero_migrate::model::ir::{IrScalar, Op, PolicyCmd};
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_policy_rollback_pg";
const PROJECT_SCHEMA: &str = "zero_migrate";
const RECORDED_SCHEMA: &str = "recorded_policy_schema";
const POLICY: &str = "tenant_isolation";
const ORDERS: &str = "orders";
const INVOICES: &str = "invoices";
const LIVE_TABLE: &str = "events";
const LIVE_POLICY: &str = "visible_events";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_policy_rollback_pg_{}_{}",
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

fn bool_expr(value: bool) -> Expr {
    Expr::Literal {
        value: IrScalar::Bool(value),
    }
}

fn orders_policy_op() -> Op {
    Op::CreatePolicy {
        name: POLICY.to_string(),
        table: ORDERS.to_string(),
        schema: Some(RECORDED_SCHEMA.to_string()),
        for_cmd: PolicyCmd::Update,
        to: Some(vec!["tenant_writer".to_string(), "public".to_string()]),
        using: bool_expr(true),
        with_check: Some(bool_expr(false)),
    }
}

fn invoices_policy_op() -> Op {
    Op::CreatePolicy {
        name: POLICY.to_string(),
        table: INVOICES.to_string(),
        schema: Some(RECORDED_SCHEMA.to_string()),
        for_cmd: PolicyCmd::Select,
        to: None,
        using: bool_expr(false),
        with_check: None,
    }
}

fn drop_policy_op(table: &str, if_exists: Option<bool>) -> Op {
    Op::DropPolicy {
        name: POLICY.to_string(),
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
        fold_ops(history, dialect, PROJECT_SCHEMA, &pol).expect("the policy history must fold");
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let mut drop = serde_json::json!({
        "op": "dropPolicy",
        "name": POLICY,
        "table": table,
        "schema": RECORDED_SCHEMA,
    });
    if let Some(if_exists) = if_exists {
        drop["ifExists"] = serde_json::json!(if_exists);
    }
    let document = serde_json::json!({
        "ir_version": 1,
        "name": format!("drop_{table}_{POLICY}"),
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
        .expect("the policy drop must lower");
    let [PlanStep::Ddl(migration)] = artifact.plan.steps.as_slice() else {
        panic!("expected one policy DDL step")
    };
    migration.clone()
}

fn orders_inverse() -> &'static str {
    "CREATE POLICY \"tenant_isolation\" ON \"recorded_policy_schema\".\"orders\" FOR UPDATE \
     TO \"tenant_writer\", PUBLIC USING (TRUE) WITH CHECK (FALSE)"
}

fn invoices_inverse() -> &'static str {
    "CREATE POLICY \"tenant_isolation\" ON \"recorded_policy_schema\".\"invoices\" FOR SELECT \
     USING (FALSE)"
}

fn create_live_policy_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_visible_events",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "setRls",
                "table": LIVE_TABLE,
                "schema": schema,
                "enabled": true,
            },
            {
                "op": "createPolicy",
                "name": LIVE_POLICY,
                "table": LIVE_TABLE,
                "schema": schema,
                "forCmd": "update",
                "to": ["public"],
                "using": {
                    "node": "binOp",
                    "op": "gt",
                    "lhs": { "node": "colRef", "name": "id" },
                    "rhs": { "node": "literal", "value": 0 }
                },
                "withCheck": {
                    "node": "unaryOp",
                    "op": "isNotNull",
                    "operand": { "node": "colRef", "name": "payload" }
                }
            }
        ],
    })
    .to_string()
}

fn drop_live_policy_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_visible_events",
        "owner_app": OWNER,
        "ops": [{
            "op": "dropPolicy",
            "name": LIVE_POLICY,
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

#[derive(Debug, PartialEq, Eq)]
struct LivePolicyDefinition {
    command: String,
    roles: String,
    using: String,
    with_check: String,
    rls_enabled: bool,
}

async fn live_policy_definition(
    session: &PgDevSession,
    schema: &str,
) -> Result<Option<LivePolicyDefinition>, String> {
    let rows = session
        .query(
            "SELECT policy.polcmd::text AS command, policy.polroles::text AS roles, \
                    pg_get_expr(policy.polqual, policy.polrelid, true) AS using_predicate, \
                    pg_get_expr(policy.polwithcheck, policy.polrelid, true) AS check_predicate, \
                    target.relrowsecurity AS rls_enabled \
             FROM pg_catalog.pg_policy policy \
             JOIN pg_catalog.pg_class target ON target.oid = policy.polrelid \
             JOIN pg_catalog.pg_namespace namespace ON namespace.oid = target.relnamespace \
             WHERE namespace.nspname = $1 AND target.relname = $2 AND policy.polname = $3",
            &[schema.into(), LIVE_TABLE.into(), LIVE_POLICY.into()],
        )
        .await
        .map_err(|error| format!("read the policy from pg_policy: {error}"))?;
    rows.first()
        .map(|row| {
            Ok(LivePolicyDefinition {
                command: row
                    .try_get::<_, String>("command")
                    .map_err(|error| format!("decode policy command: {error}"))?,
                roles: row
                    .try_get::<_, String>("roles")
                    .map_err(|error| format!("decode policy roles: {error}"))?,
                using: row
                    .try_get::<_, String>("using_predicate")
                    .map_err(|error| format!("decode policy USING predicate: {error}"))?,
                with_check: row
                    .try_get::<_, String>("check_predicate")
                    .map_err(|error| format!("decode policy WITH CHECK predicate: {error}"))?,
                rls_enabled: row
                    .try_get::<_, bool>("rls_enabled")
                    .map_err(|error| format!("decode table RLS state: {error}"))?,
            })
        })
        .transpose()
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        policy(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn positive_unguarded_drop_policy_from_folded_history_has_create_inverse() {
    let migration = lower_drop_from_history(&[orders_policy_op()], ORDERS, None);

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
async fn negative_guarded_drop_policy_from_folded_history_has_no_inverse() {
    let migration = lower_drop_from_history(&[orders_policy_op()], ORDERS, Some(true));

    assert_eq!(
        migration.up,
        r#"DROP POLICY IF EXISTS "tenant_isolation" ON "recorded_policy_schema"."orders""#
    );
    assert_eq!(migration.down, None);
}

#[compio::test]
async fn negative_unguarded_drop_policy_absent_from_history_has_no_inverse() {
    let migration = lower_drop_from_history(&[], ORDERS, None);

    assert_eq!(migration.down, None);
}

#[compio::test]
async fn positive_explicit_false_drop_policy_is_eligible_for_an_inverse() {
    let migration = lower_drop_from_history(&[orders_policy_op()], ORDERS, Some(false));

    assert_eq!(
        migration.up,
        r#"DROP POLICY "tenant_isolation" ON "recorded_policy_schema"."orders""#
    );
    assert_eq!(migration.down.as_deref(), Some(orders_inverse()));
}

#[compio::test]
async fn positive_same_named_policies_on_two_tables_restore_only_the_dropped_one() {
    let orders = orders_policy_op();
    let invoices = invoices_policy_op();
    let both = vec![orders.clone(), invoices.clone()];

    let folded_both = fold_ops(
        &both,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("both same-named policies must fold");
    assert_eq!(
        folded_both.policies.len(),
        2,
        "the snapshot key must retain same-named policies on different tables"
    );

    let orders_drop = lower_drop_from_history(&both, ORDERS, None);
    assert_eq!(orders_drop.down.as_deref(), Some(orders_inverse()));
    assert!(
        !orders_drop
            .down
            .as_deref()
            .expect("the orders policy has an inverse")
            .contains("invoices"),
        "the invoices policy must not overwrite the orders policy"
    );

    let history_after_drop = vec![orders, invoices, drop_policy_op(ORDERS, None)];
    let folded_after_drop = fold_ops(
        &history_after_drop,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("dropping one same-named policy must fold");
    assert_eq!(folded_after_drop.policies.len(), 1);
    assert_eq!(
        lower_drop_from_history(&history_after_drop, ORDERS, None).down,
        None,
        "the dropped orders policy must be absent from the folded history"
    );
    assert_eq!(
        lower_drop_from_history(&history_after_drop, INVOICES, None)
            .down
            .as_deref(),
        Some(invoices_inverse()),
        "dropping the orders policy must retain the same-named invoices policy"
    );
}

#[compio::test]
async fn positive_rolling_back_a_dropped_policy_restores_its_definition() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let quoted_table = quote_ident(LIVE_TABLE);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA {quoted_schema}; \
             CREATE TABLE {quoted_schema}.{quoted_table} \
                 (id integer PRIMARY KEY, payload text)"
        ))
        .await
        .expect("create isolated policy test objects");

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
            &create_live_policy_doc(&cfg.project_schema),
            &mut history,
            Approval::None,
        )
        .await?;
        let before = live_policy_definition(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "the policy must exist before it is dropped".to_string())?;
        if !before.rls_enabled {
            return Err("the policy fixture must enable row-level security".to_string());
        }

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_live_policy_doc(&cfg.project_schema),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if live_policy_definition(&session, &cfg.project_schema)
            .await?
            .is_some()
        {
            return Err("the drop must remove the policy".to_string());
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
        .map_err(|error| format!("rolling back the policy drop must succeed: {error}"))?;

        let after = live_policy_definition(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "rollback must restore the policy".to_string())?;
        if after != before {
            return Err(format!(
                "rollback restored the wrong policy definition\n  before: {before:?}\n   after: {after:?}"
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
