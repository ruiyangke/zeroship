//! A same-name index whose SHAPE changed is surfaced, not silently accepted.
//!
//! The declarative differ pairs a desired index against a live one by name, and on
//! an EXACT-name pair it compared exactly two facets: `unique` and `columns`. Every
//! other observable facet - access method, partial predicate, INCLUDE payload,
//! storage parameters, ONLY, catalog comment - therefore produced NO migration and
//! NO error: the plan came back clean while the live index was a different index.
//! The ALIAS arm of the same pairing already asked the whole question through
//! `IndexSnapshot::same_definition_except_name`, so the two arms of one pairing
//! disagreed about what makes an index the same index.
//!
//! The arms only mean something together: A is the reported case (a method flip),
//! B and C are non-method facets that prove the fix is not tuned to the one case
//! that motivated it, and D is the population that works today - an index that is
//! genuinely unchanged must still produce an EMPTY plan, or a fix that reports
//! drift on everything would pass.
//!
//! What the check can see is bounded by what live introspection recovers.
//! `opclass`, `nulls_not_distinct` and `expr_cascade_columns` are emission-only:
//! `snapshot_schema` never populates them, they are excluded from `IndexSnapshot`
//! equality, and they are invisible to this check too. A pair that passes it agrees
//! on everything the live snapshot observes, and nothing more.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`; skips cleanly when unset.

mod support;

use std::collections::HashMap;

use support::PgDevSession;

use zero_migrate::{
    desired_snapshot, snapshot_schema, Approval, CollectionDescriptor, DeclarativeAuthor,
    DeclarativeError, EffectivePolicy, ExecutorConfig, FieldDescriptor, GuardConfig,
    IndexDescriptor, MigrationEngine, PostgresBackend, SqlDialect,
};

/// The table every arm deploys.
const TABLE: &str = "zz_idx_shape";
/// The index every arm declares, and then re-creates live under a changed shape.
const INDEX: &str = "zz_idx_shape_body_idx";
/// The key column.
const KEY: &str = "body";
/// A second column, so arm C has something to put in an INCLUDE payload.
const PAYLOAD: &str = "extra";

fn effective_policy(cfg: &ExecutorConfig) -> EffectivePolicy {
    support::no_inject(&cfg.project_schema)
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    use zero_migrate::driver::SqlSession;
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig::from_policy(
        support::no_inject(&cfg.project_schema),
        SqlDialect::Postgres,
    )
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// The collection every arm declares: two plain columns and one plain B-tree index
/// on the first. `IndexDescriptor` carries no method / predicate / INCLUDE facet, so
/// the DESIRED index is always the default shape and the live one is what the arm
/// re-creates by hand.
fn descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: TABLE.into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: KEY.into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: PAYLOAD.into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![IndexDescriptor {
            name: INDEX.into(),
            columns: vec![KEY.into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    }
}

/// Deploy the descriptor against a live database so the table and its declared
/// B-tree index exist.
async fn deploy(session: &PgDevSession, cfg: &ExecutorConfig, engine: &MigrationEngine) {
    let author = author_for(cfg);
    let desc = descriptor();
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(cfg),
    )
    .expect("desired_snapshot");
    let live = snapshot_schema(session, &cfg.project_schema)
        .await
        .expect("snapshot live");
    let plan = engine
        .plan_declarative(
            &desired,
            &live,
            &HashMap::new(),
            &author,
            &[],
            &guard_cfg(cfg),
            &effective_policy(cfg),
        )
        .expect("plan_declarative");
    let backend = PostgresBackend::new_generic(session);
    engine
        .apply_declarative(
            &plan,
            &effective_policy(cfg),
            Approval::Approved,
            &backend,
            cfg,
            "app_test",
        )
        .await
        .expect("apply_declarative");
}

/// Replace the live index with one carrying the same NAME and a different SHAPE.
/// `tail` is spliced into `CREATE INDEX <name> ON <table> <tail>`.
async fn recreate_index(session: &PgDevSession, cfg: &ExecutorConfig, tail: &str) {
    use zero_migrate::driver::SqlSession;
    session
        .batch(&format!(
            "DROP INDEX \"{schema}\".\"{INDEX}\"; \
             CREATE INDEX \"{INDEX}\" ON \"{schema}\".\"{TABLE}\" {tail};",
            schema = cfg.project_schema,
        ))
        .await
        .expect("re-create the index under a changed shape");
}

/// The access method PostgreSQL reports for the live index, so an arm asserts the
/// live shape it set up rather than assuming the DDL landed.
async fn live_access_method(session: &PgDevSession, cfg: &ExecutorConfig) -> String {
    use zero_migrate::driver::SqlSession;
    let rows = session
        .query(
            "SELECT am.amname AS m FROM pg_class ic \
             JOIN pg_am am ON am.oid = ic.relam \
             JOIN pg_namespace n ON n.oid = ic.relnamespace \
             WHERE n.nspname = $1 AND ic.relname = $2",
            &[cfg.project_schema.as_str().into(), INDEX.into()],
        )
        .await
        .expect("read pg_am");
    rows.first()
        .expect("the index must exist")
        .try_get::<_, String>("m")
        .expect("decode amname")
}

/// Re-plan the unchanged descriptor against the live schema the arm just mutated.
async fn replan(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    engine: &MigrationEngine,
) -> Result<zero_migrate::DeclarativeDeployPlan, DeclarativeError> {
    let author = author_for(cfg);
    let desc = descriptor();
    let desired = desired_snapshot(
        &cfg.project_schema,
        std::slice::from_ref(&desc),
        &effective_policy(cfg),
    )
    .expect("desired_snapshot");
    let live = snapshot_schema(session, &cfg.project_schema)
        .await
        .expect("snapshot live (after)");
    engine.plan_declarative(
        &desired,
        &live,
        &HashMap::new(),
        &author,
        &[],
        &guard_cfg(cfg),
        &effective_policy(cfg),
    )
}

/// Assert that re-planning refused, and that the refusal names the index.
fn assert_refused(
    result: Result<zero_migrate::DeclarativeDeployPlan, DeclarativeError>,
    facet: &str,
) {
    match result {
        Err(DeclarativeError::UnsupportedInV1(msg)) => {
            assert!(
                msg.contains(INDEX) && msg.contains(facet),
                "the refusal must name the index and the facet that changed, got {msg:?}"
            );
        }
        Err(other) => panic!("expected UnsupportedInV1, got {other:?}"),
        Ok(plan) => {
            let statements: Vec<String> = plan
                .plain
                .items
                .iter()
                .map(|i| i.migration.up.clone())
                .collect();
            panic!(
                "a same-name index whose {facet} changed must not diff clean; \
                 the plan carried: {statements:#?}"
            );
        }
    }
}

/// ARM A - the reported case. The live index keeps its NAME and changes its ACCESS
/// METHOD. Nothing about the name, uniqueness or column set moved, so the two-facet
/// exact-name check saw an unchanged index and the differ planned nothing.
#[compio::test]
async fn a_access_method_change_is_surfaced() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    deploy(&session, &cfg, &engine).await;
    assert_eq!(
        live_access_method(&session, &cfg).await,
        "btree",
        "the declared index deploys as the default method"
    );

    // `hash` is a built-in method needing no extension, so this arm reproduces the
    // btree -> ivfflat flip without depending on pgvector being installed.
    recreate_index(&session, &cfg, &format!("USING hash (\"{KEY}\")")).await;
    assert_eq!(
        live_access_method(&session, &cfg).await,
        "hash",
        "the live index must really carry the other method"
    );

    assert_refused(replan(&session, &cfg, &engine).await, "access method");

    drop_schemas(&session, &cfg).await;
}

/// ARM B - a NON-METHOD facet. The live index keeps its name, uniqueness, method and
/// column set, and gains a partial PREDICATE, so it no longer covers the rows the
/// declared index covers.
#[compio::test]
async fn b_predicate_change_is_surfaced() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    deploy(&session, &cfg, &engine).await;

    recreate_index(
        &session,
        &cfg,
        &format!("(\"{KEY}\") WHERE (\"{KEY}\" <> '')"),
    )
    .await;

    assert_refused(replan(&session, &cfg, &engine).await, "predicate");

    drop_schemas(&session, &cfg).await;
}

/// ARM C - a second NON-METHOD facet. The live index keeps its name, uniqueness,
/// method, column set and predicate, and gains an INCLUDE payload, so an
/// index-only scan the declared index cannot serve now succeeds.
#[compio::test]
async fn c_include_change_is_surfaced() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    deploy(&session, &cfg, &engine).await;

    recreate_index(
        &session,
        &cfg,
        &format!("(\"{KEY}\") INCLUDE (\"{PAYLOAD}\")"),
    )
    .await;

    assert_refused(replan(&session, &cfg, &engine).await, "include");

    drop_schemas(&session, &cfg).await;
}

/// ARM D - the population that works today. An index nothing touched must still
/// re-diff to an EMPTY plan. Without this a check that reported drift on every
/// exact-name pair would pass arms A, B and C.
#[compio::test]
async fn d_unchanged_index_still_plans_nothing() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    let engine = MigrationEngine::new();
    deploy(&session, &cfg, &engine).await;

    let plan = replan(&session, &cfg, &engine)
        .await
        .expect("an unchanged index must still plan");
    let statements: Vec<String> = plan
        .plain
        .items
        .iter()
        .map(|i| i.migration.up.clone())
        .collect();
    assert!(
        plan.plain.items.is_empty(),
        "re-deploying an unchanged index must stay a no-op; plan carried: {statements:#?}"
    );
    // The exact-name arm is the one under test, so this arm must be reaching clean
    // through it and not through the alias.
    assert!(
        plan.accepted_index_aliases.is_empty(),
        "an unchanged index must pair on its exact name, got {:#?}",
        plan.accepted_index_aliases
    );

    drop_schemas(&session, &cfg).await;
}
