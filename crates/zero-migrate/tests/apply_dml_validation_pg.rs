mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::{DbError, SqlSession};
use zero_migrate::{
    ApplyError, Approval, EngineError, ExecutorConfig, LiveSchema, MigrationEngine, MigrationIr,
    PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_test";

#[derive(Debug)]
enum BoundaryResult {
    LowerRejected {
        message: String,
    },
    ServerRejected {
        sqlstate: Option<String>,
        message: String,
    },
    Applied,
    OtherError {
        message: String,
    },
}

impl BoundaryResult {
    fn is_lower_rejection(&self, code: &str) -> bool {
        matches!(self, Self::LowerRejected { message } if message.contains(code))
    }

    fn report(&self) -> String {
        match self {
            Self::LowerRejected { message } => format!("LOWER_REJECTED message={message:?}"),
            Self::ServerRejected { sqlstate, message } => format!(
                "SERVER_REJECTED sqlstate={} message={message:?}",
                sqlstate.as_deref().unwrap_or("NONE")
            ),
            Self::Applied => "APPLIED".to_string(),
            Self::OtherError { message } => format!("OTHER_ERROR message={message:?}"),
        }
    }
}

fn pg_url_with_banner() -> Option<String> {
    use std::io::Write as _;
    use std::sync::Once;

    static BANNER: Once = Once::new();
    match support::pg_url() {
        Some(url) => {
            BANNER.call_once(|| {
                let _ = writeln!(
                    std::io::stderr(),
                    "LIVE_DATABASE_BANNER=ACTIVE env={}",
                    support::PG_URL_ENV
                );
            });
            Some(url)
        }
        None => {
            BANNER.call_once(|| {
                let _ = writeln!(
                    std::io::stderr(),
                    "LIVE_DATABASE_BANNER=SKIPPED env={}",
                    support::PG_URL_ENV
                );
            });
            support::announce_live_db_skip(support::PG_URL_ENV);
            None
        }
    }
}

fn token(label: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the Unix epoch")
        .as_nanos();
    format!("{label}_{}_{}_{}", std::process::id(), nanos, sequence)
}

fn cfg_for(label: &str) -> ExecutorConfig {
    let suffix = token(label);
    let project_schema = format!("apply_dml_{suffix}");
    let mut cfg = ExecutorConfig::new(
        format!("apply_dml_project_{suffix}"),
        project_schema.clone(),
        support::no_inject(&project_schema),
    );
    cfg.pg.meta_schema = format!("apply_dml_meta_{suffix}");
    cfg
}

async fn prepare_schemas<'a>(
    session: &'a PgDevSession,
    cfg: &ExecutorConfig,
) -> support::SchemaGuard<'a> {
    session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
             DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await
        .expect("remove stale test schemas");
    let guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    guard
}

fn classify(result: Result<zero_migrate::AggregateOutcome, EngineError>) -> BoundaryResult {
    match result {
        Ok(_) => BoundaryResult::Applied,
        Err(EngineError::EnvelopeDeploy(message)) => BoundaryResult::LowerRejected { message },
        Err(EngineError::Apply(ApplyError::MigrationFailed { source, .. })) => {
            if let Some(error) = source.downcast_ref::<DbError>() {
                BoundaryResult::ServerRejected {
                    sqlstate: error.sqlstate.clone(),
                    message: error.message.clone(),
                }
            } else {
                BoundaryResult::OtherError {
                    message: format!("non-DbError migration failure: {source}"),
                }
            }
        }
        Err(error) => BoundaryResult::OtherError {
            message: error.to_string(),
        },
    }
}

async fn measure_qualified_ref(url: &str, other_present: bool) -> BoundaryResult {
    let session = PgDevSession::connect(url);
    let label = if other_present {
        "qualified_present"
    } else {
        "qualified_absent"
    };
    let cfg = cfg_for(label);
    let _schemas = prepare_schemas(&session, &cfg).await;

    session
        .batch(&format!(
            "CREATE TABLE \"{}\".\"users\" (\
                 \"id\" bigint PRIMARY KEY, \
                 \"n\" bigint NOT NULL\
             ); \
             INSERT INTO \"{}\".\"users\" (\"id\", \"n\") VALUES (1, 7);",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("seed live users table");
    if other_present {
        session
            .batch(&format!(
                "CREATE TABLE \"{}\".\"other\" (\"ghost\" bigint NOT NULL)",
                cfg.project_schema
            ))
            .await
            .expect("seed live other table");
    }

    let backend = PostgresBackend::new_generic(&session);
    let snapshot = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot the populated live schema");
    let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    assert!(
        live.table_snapshots.contains_key("users"),
        "users must be present in the live table snapshot"
    );
    assert_eq!(
        live.table_snapshots.contains_key("other"),
        other_present,
        "the live table snapshot must match the present/absent control"
    );
    eprintln!(
        "LIVE_SNAPSHOT case={label} tables={:?}",
        live.table_snapshots.keys().collect::<Vec<_>>()
    );

    let ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": format!("measure_{label}"),
        "owner_app": OWNER,
        "ops": [{
            "op": "update",
            "table": "users",
            "set": { "n": 8 },
            "where": {
                "node": "binOp",
                "op": "eq",
                "lhs": {
                    "node": "colRef",
                    "table": "other",
                    "name": "ghost"
                },
                "rhs": { "node": "literal", "value": 1 }
            }
        }]
    }))
    .expect("qualified-reference update IR parses");
    let mut registry = BTreeMap::from([("users".to_string(), OWNER.to_string())]);
    if other_present {
        registry.insert("other".to_string(), OWNER.to_string());
    }
    let policy = support::no_inject(&cfg.project_schema);
    classify(
        MigrationEngine::new()
            .deploy_envelopes(
                &[ir],
                &backend,
                &policy,
                SqlDialect::Postgres,
                &cfg.project_schema,
                OWNER,
                &registry,
                Approval::Approved,
                &cfg,
            )
            .await,
    )
}

async fn measure_aggregate_update(url: &str) -> BoundaryResult {
    let session = PgDevSession::connect(url);
    let cfg = cfg_for("aggregate");
    let _schemas = prepare_schemas(&session, &cfg).await;
    session
        .batch(&format!(
            "CREATE TABLE \"{}\".\"users\" (\
                 \"id\" bigint PRIMARY KEY, \
                 \"n\" bigint NOT NULL\
             ); \
             INSERT INTO \"{}\".\"users\" (\"id\", \"n\") VALUES (1, 7);",
            cfg.project_schema, cfg.project_schema
        ))
        .await
        .expect("seed live aggregate target");

    let backend = PostgresBackend::new_generic(&session);
    let snapshot = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot the populated aggregate target");
    let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    assert!(
        live.table_snapshots.contains_key("users"),
        "users must be present in the live table snapshot"
    );
    eprintln!(
        "LIVE_SNAPSHOT case=aggregate tables={:?}",
        live.table_snapshots.keys().collect::<Vec<_>>()
    );

    let ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "measure_aggregate_update",
        "owner_app": OWNER,
        "ops": [{
            "op": "update",
            "table": "users",
            "set": {
                "n": {
                    "node": "agg",
                    "func": "count",
                    "arg": { "node": "colRef", "name": "n" }
                }
            }
        }]
    }))
    .expect("aggregate update IR parses");
    let registry = BTreeMap::from([("users".to_string(), OWNER.to_string())]);
    let policy = support::no_inject(&cfg.project_schema);
    classify(
        MigrationEngine::new()
            .deploy_envelopes(
                &[ir],
                &backend,
                &policy,
                SqlDialect::Postgres,
                &cfg.project_schema,
                OWNER,
                &registry,
                Approval::Approved,
                &cfg,
            )
            .await,
    )
}

#[compio::test]
async fn live_server_control_proves_the_suite_reached_postgres() {
    let Some(url) = pg_url_with_banner() else {
        return;
    };
    let session = PgDevSession::connect(&url);
    let row = session
        .query_one(
            "SELECT current_database()::text AS database_name, \
                    current_setting('server_version')::text AS server_version",
            &[],
        )
        .await
        .expect("read the live PostgreSQL identity");
    let database_name: String = row.try_get("database_name").expect("decode database name");
    let server_version: String = row
        .try_get("server_version")
        .expect("decode server version");
    eprintln!("LIVE_SERVER_PROBE database={database_name} server_version={server_version}");
}

#[compio::test]
async fn qualified_dml_refs_are_rejected_before_postgres_for_present_and_absent_tables() {
    let Some(url) = pg_url_with_banner() else {
        return;
    };
    let present = measure_qualified_ref(&url, true).await;
    let absent = measure_qualified_ref(&url, false).await;
    let present_report = present.report();
    let absent_report = absent.report();
    eprintln!("MEASURE_QUALIFIED_PRESENT={present_report}");
    eprintln!("MEASURE_QUALIFIED_ABSENT={absent_report}");

    assert!(
        present.is_lower_rejection("UNSUPPORTED") && absent.is_lower_rejection("UNSUPPORTED"),
        "qualified DML references must be rejected by lowering before PostgreSQL; \
         present={present_report}; absent={absent_report}"
    );
}

#[compio::test]
async fn aggregate_update_is_rejected_before_postgres() {
    let Some(url) = pg_url_with_banner() else {
        return;
    };
    let result = measure_aggregate_update(&url).await;
    let report = result.report();
    eprintln!("MEASURE_AGGREGATE_UPDATE={report}");

    assert!(
        result.is_lower_rejection("AGGREGATE_IN_SCALAR_CONTEXT"),
        "aggregate Update assignments must be rejected by lowering before PostgreSQL; \
         result={report}"
    );
}
