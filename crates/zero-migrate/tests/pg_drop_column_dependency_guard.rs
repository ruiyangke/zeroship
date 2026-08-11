//! Live PostgreSQL coverage for plain `dropColumn` dependency preconditions.
//!
//! PostgreSQL refuses a bare column drop when objects such as views depend on the
//! column. The lowering path must turn that catalog rule into a per-migration
//! precondition so apply refuses before running the DDL and reports the blocker by
//! name. A masked logical column lowers to two independently committed physical
//! drops, so the sibling unit must query its own column rather than reuse the
//! authored column's check.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    snapshot_schema, ApplyError, Approval, DeclarativeApplyError, EngineError, ExecutorConfig,
    GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr, PlanStep,
    PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_drop_column_dependency_guard";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos();
    format!(
        "drop_column_dependency_guard_{}_{}_{}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn cfg_for(token: &str) -> ExecutorConfig {
    let schema = format!("proj_{token}");
    let mut cfg = ExecutorConfig::new(
        format!("project_{token}"),
        &schema,
        support::no_inject(&schema),
    );
    cfg.pg.meta_schema = format!("meta_{token}");
    cfg
}

/// Create the project schema and arm cleanup for both schemas before any assertion
/// can unwind. The guard uses the pinned connection and rolls back a failed
/// transaction before dropping, so a failing assertion cannot leak its fixture.
#[must_use = "the guard drops the schemas when it falls out of scope"]
async fn ensure_project_schema<'a>(
    session: &'a PgDevSession,
    cfg: &ExecutorConfig,
) -> support::SchemaGuard<'a> {
    let guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
    guard
}

/// Lower one typed drop against the actual catalog. The live snapshot is required
/// for the masked-sibling branch: a `DropColumn` op carries no mask facet, so only
/// the existing `<column>_masked` object tells lowering to emit a second unit.
async fn lower_drop_steps(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    name: &str,
    table: &str,
    column: &str,
) -> Vec<PlanStep> {
    let snapshot = snapshot_schema(session, &cfg.project_schema)
        .await
        .expect("snapshot the drop fixture");
    let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    let source = format!(
        r#"{{"ir_version":1,"name":"{name}","owner_app":"{OWNER}","ops":[
          {{"op":"dropColumn","table":"{table}","column":"{column}"}}
        ]}}"#
    );
    let authored: MigrationIr = serde_json::from_str(&source).expect("parse dropColumn IR");
    let registry = BTreeMap::from([(table.to_string(), OWNER.to_string())]);
    let policy = support::no_inject(&cfg.project_schema);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy)
        .load_and_lower_guarded(
            &serde_json::to_string(&authored).expect("serialize dropColumn IR"),
            OWNER,
            &registry,
            &live,
            &guard,
        )
        .expect("load and lower dropColumn plan")
        .plan
        .steps
}

async fn apply_steps(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    steps: &[PlanStep],
) -> Result<(), DeclarativeApplyError> {
    let backend = PostgresBackend::new_generic(session);
    MigrationEngine::new()
        .apply_plan(
            steps,
            Approval::Approved,
            &backend,
            cfg,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
}

fn assert_named_precondition_refusal(
    error: DeclarativeApplyError,
    table: &str,
    column: &str,
    blocker: &str,
) {
    let DeclarativeApplyError::Plain(EngineError::Apply(ApplyError::PreconditionFailed {
        which,
        ..
    })) = error
    else {
        panic!("expected a structured precondition refusal, got {error:#?}");
    };
    assert!(
        which.contains(table) && which.contains(column),
        "the refusal must identify the guarded column {table}.{column}: {which}"
    );
    assert!(
        which.contains(blocker),
        "the refusal must name blocking object {blocker}: {which}"
    );
}

async fn column_exists(session: &PgDevSession, schema: &str, table: &str, column: &str) -> bool {
    session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3) AS present",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .expect("query column existence")
        .try_get::<_, bool>("present")
        .expect("decode column existence")
}

#[compio::test]
async fn plain_drop_column_refuses_a_blocking_view_and_names_it() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = ensure_project_schema(&session, &cfg).await;

    session
        .batch(&format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY, email text); \
             CREATE VIEW \"{}\".email_reader AS \
             SELECT email FROM \"{}\".accounts",
            cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create the blocking-view fixture");

    let steps = lower_drop_steps(
        &session,
        &cfg,
        "drop_email_with_reader",
        "accounts",
        "email",
    )
    .await;
    let error = apply_steps(&session, &cfg, &steps)
        .await
        .expect_err("a view reading the column must refuse the drop");
    assert_named_precondition_refusal(error, "accounts", "email", "email_reader");
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "email").await,
        "the precondition refusal must leave the blocked column in place"
    );
}

#[compio::test]
async fn masked_drop_column_checks_the_sibling_unit_and_names_its_blocker() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = ensure_project_schema(&session, &cfg).await;

    session
        .batch(&format!(
            "CREATE TABLE \"{}\".accounts ( \
               id bigint PRIMARY KEY, ssn text, ssn_masked text \
             ); \
             COMMENT ON COLUMN \"{}\".accounts.ssn_masked IS \
               'zero-migrate:mask:kind=last4,classification=pii'; \
             CREATE VIEW \"{}\".masked_reader AS \
               SELECT ssn_masked FROM \"{}\".accounts",
            cfg.project_schema, cfg.project_schema, cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("create the masked-sibling blocker fixture");

    let steps = lower_drop_steps(
        &session,
        &cfg,
        "drop_masked_ssn_with_reader",
        "accounts",
        "ssn",
    )
    .await;
    assert_eq!(
        steps
            .iter()
            .filter(|step| matches!(step, PlanStep::Ddl(_)))
            .count(),
        2,
        "the live masked sibling must lower as its own DDL unit"
    );

    let error = apply_steps(&session, &cfg, &steps)
        .await
        .expect_err("the view reading the masked sibling must refuse its drop");
    assert_named_precondition_refusal(error, "accounts", "ssn_masked", "masked_reader");
    assert!(
        !column_exists(&session, &cfg.project_schema, "accounts", "ssn").await,
        "the main unit committed before the sibling unit was checked"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", "ssn_masked").await,
        "the sibling precondition must refuse before its own DDL runs"
    );
}

#[compio::test]
async fn plain_drop_column_without_blockers_applies_with_its_guard() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = ensure_project_schema(&session, &cfg).await;

    session
        .batch(&format!(
            "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY, obsolete text)",
            cfg.project_schema
        ))
        .await
        .expect("create the unblocked-drop fixture");

    let steps = lower_drop_steps(
        &session,
        &cfg,
        "drop_unblocked_obsolete",
        "accounts",
        "obsolete",
    )
    .await;
    apply_steps(&session, &cfg, &steps)
        .await
        .expect("a drop with no blocking dependents applies");
    assert!(
        !column_exists(&session, &cfg.project_schema, "accounts", "obsolete").await,
        "the unblocked column must be dropped"
    );

    let ddl = steps
        .iter()
        .find_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration),
            _ => None,
        })
        .expect("dropColumn lowers to a DDL migration");
    assert_eq!(
        ddl.preconditions.len(),
        1,
        "a successful PostgreSQL drop must still have evaluated the dependency guard"
    );
}
