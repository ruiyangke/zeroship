//! The host-selected migration target is independent of foreign-schema grants.

#[path = "../../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

use std::collections::BTreeMap;

use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::guard::{GuardConfig, GuardError};
use zeroship_migrate::render::lower::{IrAuthor, LiveSchema, LoweredArtifact};
use zeroship_migrate::{Approval, EffectivePolicy, ExecutorConfig, LockMode, MigrationEngine};
use zeroship_migrate_backend::executor::authorize_existence_guard_schema;
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_postgres::{guard::SqlGuard, PostgresBackend};

const PROJECT: &str = "public";
const OWNER: &str = "app_schema_scope";

fn policy(foreign: Option<&str>, create: bool) -> EffectivePolicy {
    let mut charter = String::from("policy_version = 1\n");
    if create {
        charter
            .push_str("[[grant]]\nkey = \"schema.create_table\"\nvalue = true\nscope = \"all\"\n");
    }
    if let Some(foreign) = foreign {
        charter.push_str(&format!(
            "[[grant]]\nkey = \"schema.cross_schema\"\nvalue = true\nscope = {{ include = [{foreign:?}] }}\n"
        ));
    }
    zeroship_migrate::effective_policy_from_charter_toml(&charter).expect("policy composes")
}

fn cfg(foreign: Option<&str>, create: bool) -> ExecutorConfig {
    ExecutorConfig::new("schema_scope", PROJECT, policy(foreign, create))
}

fn lower(
    config: &ExecutorConfig,
    dialect: &DialectId,
    schema: &str,
) -> Result<LoweredArtifact, String> {
    let guard = config.guard_config_for(dialect);
    let source = serde_json::json!({
        "ir_version": 1, "name": "create_rows", "owner_app": OWNER,
        "ops": [
            {"op": "createTable", "name": "scope_rows", "schema": schema,
             "columns": [{"name": "id", "type": "int", "nullable": false},
                         {"name": "value", "type": "int", "nullable": false}],
             "primaryKey": ["id"]},
            {"op": "createIndex", "table": "scope_rows", "schema": schema,
             "name": "scope_rows_value_idx", "columns": [{"kind": "column", "name": "value"}]}
        ]
    })
    .to_string();
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        OWNER,
        dialect,
        guard.effective(),
    )
    .load_and_lower_guarded(
        &source,
        OWNER,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &guard,
    )
    .map_err(|error| error.to_string())
}

#[test]
fn target_and_foreign_authority_agree_for_sql_ir_and_probes() {
    for dialect in [
        zeroship_migrate_postgres::DIALECT,
        zeroship_migrate_sqlite::DIALECT,
    ] {
        let confined = cfg(None, true);
        let guard = confined.guard_config_for(&dialect);
        assert_eq!(guard.pinned_schema().as_deref(), Some(PROJECT));
        assert!(guard.permits_schema(PROJECT));
        assert!(!guard.permits_schema("analytics"));
        lower(&confined, &dialect, PROJECT).expect("own-schema IR is authorized");
        assert!(lower(&confined, &dialect, "analytics").is_err());
        authorize_existence_guard_schema(&confined, "probe", PROJECT, &dialect).unwrap();
        assert!(
            authorize_existence_guard_schema(&confined, "probe", "analytics", &dialect).is_err()
        );

        let granted = cfg(Some("analytics"), true);
        let guard = granted.guard_config_for(&dialect);
        assert_eq!(guard.pinned_schema().as_deref(), Some(PROJECT));
        assert!(guard.permits_schema(PROJECT));
        assert!(guard.permits_schema("analytics"));
        assert!(!guard.permits_schema("private"));
        authorize_existence_guard_schema(&granted, "probe", "analytics", &dialect).unwrap();
        lower(&granted, &dialect, PROJECT).expect("foreign grant preserves own-schema authority");
        // SQLite has no foreign application schemas; PostgreSQL can render one.
        if dialect == zeroship_migrate_postgres::DIALECT {
            lower(&granted, &dialect, "analytics").expect("explicit foreign grant authorizes IR");
        }
        let refusal = lower(&cfg(None, false), &dialect, PROJECT)
            .expect_err("own-schema access does not grant table creation");
        assert!(refusal.contains("CreateTableNotGranted"), "{refusal}");
    }
    let guard =
        SqlGuard::new(cfg(None, true).guard_config_for(&zeroship_migrate_postgres::DIALECT));
    guard
        .check("CREATE TABLE public.rows (id integer)")
        .unwrap();
    guard.check("CREATE TABLE rows (id integer)").unwrap();
    assert!(matches!(
        guard.check("SELECT * FROM analytics.rows"),
        Err(GuardError::CrossSchema { .. })
    ));
    let guard = SqlGuard::new(
        cfg(Some("analytics"), false).guard_config_for(&zeroship_migrate_postgres::DIALECT),
    );
    guard.check("SELECT * FROM analytics.rows").unwrap();
    assert!(matches!(
        guard.check("CREATE TABLE public.rows (id integer)"),
        Err(GuardError::NamespacePolicy { .. })
    ));
    assert!(matches!(
        guard.check("CREATE TABLE analytics.rows (id integer)"),
        Err(GuardError::NamespacePolicy { .. })
    ));
}

#[test]
fn replacing_policy_preserves_the_host_target() {
    let guard = GuardConfig::from_policy(
        policy(None, true),
        zeroship_migrate_postgres::DIALECT,
        PROJECT,
    )
    .with_effective_policy(policy(Some("analytics"), true));
    assert_eq!(guard.project_schema(), PROJECT);
    assert_eq!(guard.pinned_schema().as_deref(), Some(PROJECT));
    assert!(guard.permits_schema(PROJECT));
    assert!(guard.permits_schema("analytics"));
}

async fn apply_and_reapply<B: MigrationBackend>(backend: &B, dialect: &DialectId) {
    let config = cfg(None, true);
    let artifact =
        lower(&config, dialect, PROJECT).expect("local IR lowers without a foreign grant");
    let engine = MigrationEngine::new(zeroship_migrate::shipping_vendors());
    let first = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            &config,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .expect("local migration applies without a foreign grant");
    assert!(!first.applied.applied.is_empty());
    let repeated = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            &config,
            OWNER,
            LockMode::Acquire,
        )
        .await
        .expect("local migration reapplication succeeds");
    assert!(repeated.applied.applied.is_empty());
}

#[compio::test]
async fn sqlite_file_applies_and_reapplies_without_cross_schema_grants() {
    let directory = tempfile::tempdir().unwrap();
    let application = directory.path().join("application.sqlite");
    let backend = zeroship_migrate_sqlite::SqliteBackend::open(&application).unwrap();
    apply_and_reapply(&backend, &zeroship_migrate_sqlite::DIALECT).await;
    let connection = rusqlite::Connection::open(&application).unwrap();
    let indexes: String = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'scope_rows_value_idx'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, "scope_rows_value_idx");
}

#[compio::test]
async fn postgres_applies_and_reapplies_without_cross_schema_grants() {
    let postgres = postgres_fixture::Postgres::start();
    let session = crate::support::PgDevSession::connect(&postgres.url());
    let backend = PostgresBackend::new_generic(&session);
    apply_and_reapply(&backend, &zeroship_migrate_postgres::DIALECT).await;
    let rows = session.query("SELECT indexname FROM pg_indexes WHERE schemaname = 'public' AND indexname = 'scope_rows_value_idx'", &[]).await.unwrap();
    assert!(!rows.is_empty());
}

#[test]
fn foreign_globs_and_exclusions_have_the_same_authority_at_every_gate() {
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        r#"policy_version = 1
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["analytics*"], exclude = ["analytics_private"] }
"#,
    )
    .unwrap();
    let config = ExecutorConfig::new("schema_scope", PROJECT, policy);
    let dialect = zeroship_migrate_postgres::DIALECT;
    let guard = config.guard_config_for(&dialect);
    let scope = guard.schema_scope().unwrap();
    let sql_guard = SqlGuard::new(guard.clone());
    for (schema, admitted) in [
        (PROJECT, true),
        ("analytics_reports", true),
        ("analytics_private", false),
        ("private", false),
    ] {
        assert_eq!(
            guard.permits_schema(schema),
            admitted,
            "SQL authority for {schema}"
        );
        assert_eq!(scope.permits(schema), admitted, "IR authority for {schema}");
        assert_eq!(
            authorize_existence_guard_schema(&config, "probe", schema, &dialect).is_ok(),
            admitted,
            "probe authority for {schema}"
        );
        assert_eq!(
            lower(&config, &dialect, schema).is_ok(),
            admitted,
            "IR lowering for {schema}"
        );
        assert_eq!(
            sql_guard
                .check(&format!("SELECT * FROM {schema}.rows"))
                .is_ok(),
            admitted,
            "SQL parsing for {schema}"
        );
    }
}

#[test]
fn local_table_rename_requires_its_own_operation_grant() {
    let source = serde_json::json!({
        "ir_version": 1, "name": "rename_rows", "owner_app": OWNER,
        "ops": [
            {"op": "createTable", "name": "rows", "columns": [{"name": "id", "type": "int"}]},
            {"op": "renameTable", "table": "rows", "to": "renamed_rows"}
        ]
    })
    .to_string();
    for dialect in [
        zeroship_migrate_postgres::DIALECT,
        zeroship_migrate_sqlite::DIALECT,
    ] {
        for rename_granted in [false, true] {
            let mut charter = String::from("policy_version = 1\n[[grant]]\nkey = \"schema.create_table\"\nvalue = true\nscope = \"all\"\n");
            if rename_granted {
                charter.push_str("[[grant]]\nkey = \"schema.rename\"\nvalue = true\nscope = { include = [\"public.renamed_rows\"] }\n");
            }
            let policy = zeroship_migrate::effective_policy_from_charter_toml(&charter).unwrap();
            let guard = GuardConfig::from_policy(policy.clone(), dialect.clone(), PROJECT);
            let result = IrAuthor::new(
                zeroship_migrate::shipping_vendors(),
                PROJECT,
                OWNER,
                &dialect,
                &policy,
            )
            .load_and_lower_guarded(
                &source,
                OWNER,
                &BTreeMap::new(),
                &LiveSchema::default(),
                &guard,
            );
            if rename_granted {
                result.expect("scoped rename grant authorizes the local operation");
            } else {
                let error = result.expect_err("target-schema access does not grant rename");
                assert!(
                    error.to_string().contains("RenameIntoNotGranted"),
                    "{error}"
                );
            }
        }
    }
}
