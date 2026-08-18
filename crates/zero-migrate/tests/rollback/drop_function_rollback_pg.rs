//! Function-drop rollback lowering from folded migration history.
//!
//! PostgreSQL functions are identified by name and input argument types, so the
//! history must retain overloads independently. These tests lower through the
//! guarded authoring path with a `LiveSchema` built from `fold_ops`, matching the
//! deploy path that refreshes historical live state. The live rollback case is
//! gated behind `ZERO_MIGRATE_TEST_PG_URL`; the focused controls always run.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{FuncArg, FuncLanguage, FuncVolatility, Op};
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    fold_ops, guard_for, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_function_rollback_pg";
const PROJECT_SCHEMA: &str = "zero_migrate";
const FUNCTION: &str = "format_value";
const INTEGER_BODY: &str = "SELECT (value + 1)::text";
const TEXT_BODY: &str = "SELECT upper(value)";
const REPLACEMENT_BODY: &str = "SELECT (value + 2)::text";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drop_function_rollback_pg_{}_{}",
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

fn create_function_op(arg_type: &str, volatility: FuncVolatility, body: &str) -> Op {
    Op::CreateFunction {
        name: FUNCTION.to_string(),
        schema: Some(PROJECT_SCHEMA.to_string()),
        args: Some(vec![FuncArg {
            name: Some("value".to_string()),
            ty: arg_type.to_string(),
            mode: None,
        }]),
        returns: "text".to_string(),
        language: FuncLanguage::Sql,
        replace: None,
        volatility: Some(volatility),
        body: body.to_string(),
    }
}

fn drop_function_op(arg_type: &str, if_exists: Option<bool>) -> Op {
    Op::DropFunction {
        name: FUNCTION.to_string(),
        schema: Some(PROJECT_SCHEMA.to_string()),
        arg_types: Some(vec![arg_type.to_string()]),
        if_exists,
    }
}

fn replace_function_op(arg_type: &str, body: &str) -> Op {
    let mut op = create_function_op(arg_type, FuncVolatility::Stable, body);
    let Op::CreateFunction { replace, .. } = &mut op else {
        unreachable!("the helper constructs a createFunction op")
    };
    *replace = Some(true);
    op
}

fn lower_drop_from_history(
    history: &[Op],
    arg_types: &[&str],
    if_exists: Option<bool>,
) -> Migration {
    let dialect = SqlDialect::Postgres;
    let pol = policy(PROJECT_SCHEMA);
    let folded =
        fold_ops(history, dialect, PROJECT_SCHEMA, &pol).expect("the function history must fold");
    let live = LiveSchema::from_catalog_snapshot(folded, OWNER);
    let mut drop = serde_json::json!({
        "op": "dropFunction",
        "name": FUNCTION,
        "schema": PROJECT_SCHEMA,
        "argTypes": arg_types,
    });
    if let Some(if_exists) = if_exists {
        drop["ifExists"] = serde_json::json!(if_exists);
    }
    let document = serde_json::json!({
        "ir_version": 1,
        "name": "drop_format_value",
        "owner_app": OWNER,
        "ops": [drop],
    })
    .to_string();
    let guard = GuardConfig::from_policy(pol.clone(), dialect);
    let artifact = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &pol)
        .load_and_lower_guarded(&document, OWNER, &BTreeMap::new(), &live, &guard)
        .expect("the function drop must lower");
    let [PlanStep::Ddl(migration)] = artifact.plan.steps.as_slice() else {
        panic!("expected one function DDL step")
    };
    migration.clone()
}

fn integer_inverse() -> &'static str {
    "CREATE FUNCTION \"zero_migrate\".\"format_value\"(\"value\" integer) RETURNS text \
     LANGUAGE sql IMMUTABLE AS $zsfn$\nSELECT (value + 1)::text\n$zsfn$"
}

fn text_inverse() -> &'static str {
    "CREATE FUNCTION \"zero_migrate\".\"format_value\"(\"value\" text) RETURNS text \
     LANGUAGE sql STABLE AS $zsfn$\nSELECT upper(value)\n$zsfn$"
}

fn create_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_format_value",
        "owner_app": OWNER,
        "ops": [{
            "op": "createFunction",
            "name": FUNCTION,
            "schema": schema,
            "args": [{ "name": "value", "type": "integer" }],
            "returns": "text",
            "language": "sql",
            "volatility": "immutable",
            "body": INTEGER_BODY,
        }],
    })
    .to_string()
}

fn drop_doc(schema: &str) -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_format_value",
        "owner_app": OWNER,
        "ops": [{
            "op": "dropFunction",
            "name": FUNCTION,
            "schema": schema,
            "argTypes": ["integer"],
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
            PlanStep::Ddl(migration) => Some(migration.clone()),
            _ => None,
        })
        .collect())
}

async fn live_function_body(
    session: &PgDevSession,
    schema: &str,
) -> Result<Option<String>, String> {
    let rows = session
        .query(
            "SELECT p.prosrc AS src FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.proname = $2",
            &[schema.into(), FUNCTION.into()],
        )
        .await
        .map_err(|error| format!("read the function from pg_proc: {error}"))?;
    Ok(rows
        .first()
        .and_then(|row| row.try_get::<_, String>("src").ok()))
}

fn pg_guard(cfg: &ExecutorConfig) -> Box<dyn zero_migrate::MigrationGuard> {
    guard_for(&GuardConfig::from_policy(
        policy(&cfg.project_schema),
        SqlDialect::Postgres,
    ))
}

#[compio::test]
async fn unguarded_drop_function_from_folded_history_has_create_inverse() {
    let migration = lower_drop_from_history(
        &[create_function_op(
            "integer",
            FuncVolatility::Immutable,
            INTEGER_BODY,
        )],
        &["integer"],
        None,
    );

    assert_eq!(migration.down.as_deref(), Some(integer_inverse()));
    guard_for(&GuardConfig::from_policy(
        policy(PROJECT_SCHEMA),
        SqlDialect::Postgres,
    ))
    .as_ref()
    .check(migration.down.as_deref().expect("the inverse exists"))
    .expect("the synthesised inverse must pass the configured guard");
}

#[compio::test]
async fn guarded_drop_function_from_folded_history_has_no_inverse() {
    let migration = lower_drop_from_history(
        &[create_function_op(
            "integer",
            FuncVolatility::Immutable,
            INTEGER_BODY,
        )],
        &["integer"],
        Some(true),
    );

    assert_eq!(
        migration.up,
        r#"DROP FUNCTION IF EXISTS "zero_migrate"."format_value"(integer)"#
    );
    assert_eq!(migration.down, None);
}

#[compio::test]
async fn unguarded_drop_function_absent_from_history_has_no_inverse() {
    let migration = lower_drop_from_history(&[], &["integer"], None);

    assert_eq!(migration.down, None);
}

#[compio::test]
async fn explicit_false_drop_function_is_eligible_for_an_inverse() {
    let migration = lower_drop_from_history(
        &[create_function_op(
            "integer",
            FuncVolatility::Immutable,
            INTEGER_BODY,
        )],
        &["integer"],
        Some(false),
    );

    assert_eq!(
        migration.up,
        r#"DROP FUNCTION "zero_migrate"."format_value"(integer)"#
    );
    assert_eq!(migration.down.as_deref(), Some(integer_inverse()));
}

#[compio::test]
async fn dropping_one_overload_restores_only_that_overload() {
    let integer = create_function_op("integer", FuncVolatility::Immutable, INTEGER_BODY);
    let text = create_function_op("text", FuncVolatility::Stable, TEXT_BODY);
    let overloads = vec![integer.clone(), text.clone()];

    let folded_overloads = fold_ops(
        &overloads,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("both function overloads must fold");
    assert_eq!(
        folded_overloads.functions.len(),
        2,
        "the snapshot key must retain both same-name overloads"
    );

    let migration = lower_drop_from_history(&overloads, &["integer"], None);
    assert_eq!(migration.down.as_deref(), Some(integer_inverse()));
    assert!(
        !migration
            .down
            .as_deref()
            .expect("the integer overload has an inverse")
            .contains(TEXT_BODY),
        "the text overload must not overwrite the integer overload"
    );

    let history_after_drop = vec![integer, text, drop_function_op("integer", None)];
    let folded_after_drop = fold_ops(
        &history_after_drop,
        SqlDialect::Postgres,
        PROJECT_SCHEMA,
        &policy(PROJECT_SCHEMA),
    )
    .expect("dropping one overload must fold");
    assert_eq!(folded_after_drop.functions.len(), 1);
    let remaining = folded_after_drop
        .functions
        .values()
        .next()
        .expect("exactly one overload must remain");
    assert_eq!(remaining.body, TEXT_BODY);
    assert_eq!(
        lower_drop_from_history(&history_after_drop, &["integer"], None).down,
        None,
        "the dropped integer overload must be absent from the folded history"
    );
    assert_eq!(
        lower_drop_from_history(&history_after_drop, &["text"], None)
            .down
            .as_deref(),
        Some(text_inverse()),
        "dropping the integer overload must retain the text overload"
    );
}

#[compio::test]
async fn alias_spelled_replace_never_leaves_a_stale_inverse() {
    let history = vec![
        create_function_op("integer", FuncVolatility::Immutable, INTEGER_BODY),
        replace_function_op("int4", REPLACEMENT_BODY),
    ];

    assert_eq!(
        lower_drop_from_history(&history, &["integer"], None).down,
        None,
        "an equivalent spelling must not retrieve the body from before the replace"
    );
    assert_eq!(
        lower_drop_from_history(&history, &["int4"], None)
            .down
            .as_deref(),
        Some(
            "CREATE FUNCTION \"zero_migrate\".\"format_value\"(\"value\" int4) RETURNS text \
             LANGUAGE sql STABLE AS $zsfn$\nSELECT (value + 2)::text\n$zsfn$"
        )
    );
}

#[compio::test]
async fn alias_spelled_drop_never_leaves_a_stale_inverse() {
    let history = vec![
        create_function_op("integer", FuncVolatility::Immutable, INTEGER_BODY),
        drop_function_op("int4", None),
        create_function_op("int4", FuncVolatility::Stable, REPLACEMENT_BODY),
    ];

    assert_eq!(
        lower_drop_from_history(&history, &["integer"], None).down,
        None,
        "a later drop must not retrieve the body removed under an equivalent spelling"
    );
    assert_eq!(
        lower_drop_from_history(&history, &["int4"], None)
            .down
            .as_deref(),
        Some(
            "CREATE FUNCTION \"zero_migrate\".\"format_value\"(\"value\" int4) RETURNS text \
             LANGUAGE sql STABLE AS $zsfn$\nSELECT (value + 2)::text\n$zsfn$"
        )
    );
}

#[compio::test]
async fn rolling_back_a_dropped_function_restores_its_body() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy(&schema));
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
            &create_doc(&cfg.project_schema),
            &mut history,
            Approval::None,
        )
        .await?;
        let before = live_function_body(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "the function must exist before it is dropped".to_string())?;

        migrations.extend(
            apply_doc(
                &session,
                &cfg,
                &drop_doc(&cfg.project_schema),
                &mut history,
                Approval::Approved,
            )
            .await?,
        );
        if live_function_body(&session, &cfg.project_schema)
            .await?
            .is_some()
        {
            return Err("the drop must remove the function".to_string());
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
        .map_err(|error| format!("rolling back the function drop must succeed: {error}"))?;

        let after = live_function_body(&session, &cfg.project_schema)
            .await?
            .ok_or_else(|| "rollback must restore the function".to_string())?;
        if after != before {
            return Err(format!(
                "rollback restored the wrong function body\n  before: {before:?}\n   after: {after:?}"
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
