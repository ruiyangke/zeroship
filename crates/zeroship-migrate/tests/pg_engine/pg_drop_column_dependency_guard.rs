//! Live PostgreSQL coverage for plain `dropColumn` dependency preconditions.
//!
//! PostgreSQL refuses a bare column drop when objects such as views depend on the
//! column. The lowering path must turn that catalog rule into a per-migration
//! precondition so apply refuses before running the DDL and reports the blocker by
//! name. A masked logical column lowers to two independently committed physical
//! drops, so the sibling unit must query its own column rather than reuse the
//! authored column's check.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::{
    ApplyError, Approval, DeclarativeApplyError, EngineError, ExecutorConfig, GuardConfig,
    IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr, PlanStep,
};
use zeroship_migrate_postgres::backend::drift_sql::snapshot_schema;
use zeroship_migrate_postgres::PostgresBackend;

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
    cfg.confinement.meta_schema = format!("meta_{token}");
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
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
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
    let guard = GuardConfig::from_policy(policy.clone(), zeroship_migrate_postgres::DIALECT);
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        &cfg.project_schema,
        OWNER,
        &zeroship_migrate_postgres::DIALECT,
        &policy,
    )
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
    MigrationEngine::new(zeroship_migrate::shipping_vendors())
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
    let url = require_live_pg!();
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
    let url = require_live_pg!();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = ensure_project_schema(&session, &cfg).await;

    // The layout the platform emits AFTER the storage flip: the field's own
    // column holds the mask and carries the sentinel; `__zs_raw__ssn` holds the
    // real value. The pair is named the other way round from what this fixture
    // used to hand-write, and the drop's two-unit shape is unchanged by that -
    // which is the point of restating the fixture rather than leaving it
    // describing a layout nothing produces.
    //
    // `sensitive_column` is the one the view blocks, so it is the one the drop
    // must refuse on; `field_column` is the one the drop starts from.
    let field_column = "ssn";
    let sensitive_column = zeroship_migrate::schema::query::raw_column_name(field_column);
    session
        .batch(&format!(
            "CREATE TABLE \"{schema}\".accounts ( \
               id bigint PRIMARY KEY, {field_column} text, {sensitive_column} text \
             ); \
             COMMENT ON COLUMN \"{schema}\".accounts.{field_column} IS \
               'zero-migrate:mask:kind=last4,classification=pii'; \
             CREATE VIEW \"{schema}\".masked_reader AS \
               SELECT {sensitive_column} FROM \"{schema}\".accounts",
            schema = cfg.project_schema,
        ))
        .await
        .expect("create the masked-pair blocker fixture");

    let steps = lower_drop_steps(
        &session,
        &cfg,
        "drop_masked_ssn_with_reader",
        "accounts",
        field_column,
    )
    .await;
    assert_eq!(
        steps
            .iter()
            .filter(|step| matches!(step, PlanStep::Ddl(_)))
            .count(),
        2,
        "the live masked pair must lower as two DDL units"
    );

    let error = apply_steps(&session, &cfg, &steps)
        .await
        .expect_err("the view reading the raw column must refuse its drop");
    assert_named_precondition_refusal(error, "accounts", &sensitive_column, "masked_reader");
    assert!(
        !column_exists(&session, &cfg.project_schema, "accounts", field_column).await,
        "the first unit committed before the second unit was checked"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "accounts", &sensitive_column).await,
        "the blocked unit's precondition must refuse before its own DDL runs"
    );
}

#[compio::test]
async fn plain_drop_column_without_blockers_applies_with_its_guard() {
    let url = require_live_pg!();
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
