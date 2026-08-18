//! Exact PostgreSQL/SQLite column-collation introspection, drift, and composite-FK checks.

use crate::support;

use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::{
    diff_snapshots, snapshot_schema, IrAuthor, LiveSchema, SqlDialect, SqliteBackend,
};

const OWNER: &str = "app_collation_introspection";

fn composite_fk_ir(name: &str) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": [{
            "op": "addConstraint",
            "table": "children",
            "constraint": {
                "name": "children_parents_fk",
                "kind": {
                    "kind": "fk",
                    "columns": ["parent_tenant", "parent_entity"],
                    "referencesTable": "parents",
                    "referencesColumns": ["tenant", "entity"]
                }
            }
        }]
    }))
    .expect("composite-FK fixture parses")
}

struct SqlitePaths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn sqlite_paths() -> SqlitePaths {
    let dir = tempfile::tempdir().expect("tempdir");
    SqlitePaths {
        app: dir.path().join("app.sqlite"),
        journal: dir.path().join("journal.sqlite"),
        _dir: dir,
    }
}

fn sqlite_migration(up: &str) -> Migration {
    let flags = MigrationFlags::default();
    Migration {
        version: MigrationId::generate(),
        name: "collation_fixture".to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: OWNER,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: OWNER.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

#[compio::test]
async fn sqlite_exact_collation_is_introspected_drifted_and_rejected_for_composite_fk() {
    let paths = sqlite_paths();
    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("open SQLite backend");
    backend
        .apply_one_additive(
            &sqlite_migration(
                "CREATE TABLE parents (\
                     tenant TEXT COLLATE BINARY NOT NULL, \
                     entity TEXT NOT NULL, \
                     CONSTRAINT parents_key UNIQUE (tenant, entity)); \
                 CREATE TABLE children (\
                     parent_tenant TEXT COLLATE BINARY, \
                     parent_entity TEXT);",
            ),
            "deployer",
        )
        .await
        .expect("create SQLite collation fixture");

    let expected = backend
        .snapshot_schema_sqlite()
        .await
        .expect("initial snapshot");
    let binary = expected.tables["children"]
        .columns
        .iter()
        .find(|column| column.name == "parent_tenant")
        .expect("child tenant column");
    assert_eq!(
        binary.collation, None,
        "SQLite's explicit BINARY spelling canonicalizes to its default identity"
    );

    drop(backend);
    rusqlite::Connection::open(&paths.app)
        .expect("open raw app database")
        .execute_batch(
            "DROP TABLE children; \
             CREATE TABLE children (\
                 parent_tenant TEXT COLLATE RTRIM, \
                 parent_entity TEXT);",
        )
        .expect("change the live child collation out of band");

    let backend = SqliteBackend::open(&paths.app, &paths.journal).expect("reopen SQLite backend");
    let actual = backend
        .snapshot_schema_sqlite()
        .await
        .expect("changed snapshot");
    let rtrim = actual.tables["children"]
        .columns
        .iter()
        .find(|column| column.name == "parent_tenant")
        .and_then(|column| column.collation.as_ref())
        .expect("RTRIM identity is retained");
    assert_eq!(rtrim.schema, None);
    assert_eq!(rtrim.name, "RTRIM");

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.altered_objects.iter().any(|altered| {
            altered.object == "column parent_tenant"
                && altered.field == "collation"
                && altered.expected.is_empty()
                && altered.actual == "RTRIM"
        }),
        "SQLite collation change must surface as column drift: {drift:?}"
    );

    let live = LiveSchema::from_catalog_snapshot(actual, OWNER);
    let error = IrAuthor::new(
        "main",
        OWNER,
        SqlDialect::Sqlite,
        &support::no_inject("main"),
    )
    .lower(&composite_fk_ir("sqlite_collation_fk"), &live)
    .expect_err("different live SQLite collations must reject a composite FK");
    assert!(
        error
            .to_string()
            .contains("exact catalog collation differs")
            && error.to_string().contains("RTRIM local")
            && error.to_string().contains("default target"),
        "unexpected SQLite collation diagnostic: {error}"
    );
}

fn pg_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "collation_fk_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

#[compio::test]
async fn postgres_exact_collation_is_introspected_drifted_and_rejected_for_composite_fk() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = pg_token();
    // Dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create isolated PostgreSQL schema");

    let result: Result<(), String> = async {
        session
            .batch(&format!(
                "CREATE TABLE \"{schema}\".parents (\
                     tenant text COLLATE \"C\" NOT NULL, \
                     entity text NOT NULL, \
                     CONSTRAINT parents_key UNIQUE (tenant, entity)); \
                 CREATE TABLE \"{schema}\".children (\
                     parent_tenant text COLLATE \"C\", \
                     parent_entity text);"
            ))
            .await
            .map_err(|error| format!("create PostgreSQL collation fixture: {error}"))?;

        let expected = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("initial PostgreSQL snapshot: {error}"))?;
        let c_collation = expected.tables["children"]
            .columns
            .iter()
            .find(|column| column.name == "parent_tenant")
            .and_then(|column| column.collation.as_ref())
            .ok_or_else(|| "PostgreSQL snapshot omitted the explicit C collation".to_string())?;
        if c_collation.schema.as_deref() != Some("pg_catalog") || c_collation.name != "C" {
            return Err(format!("unexpected PostgreSQL C identity: {c_collation:?}"));
        }

        session
            .batch(&format!(
                "DROP TABLE \"{schema}\".children; \
                 CREATE TABLE \"{schema}\".children (\
                     parent_tenant text COLLATE \"POSIX\", \
                     parent_entity text);"
            ))
            .await
            .map_err(|error| format!("change PostgreSQL child collation: {error}"))?;
        let actual = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("changed PostgreSQL snapshot: {error}"))?;
        let posix = actual.tables["children"]
            .columns
            .iter()
            .find(|column| column.name == "parent_tenant")
            .and_then(|column| column.collation.as_ref())
            .ok_or_else(|| "PostgreSQL snapshot omitted POSIX collation".to_string())?;
        if posix.schema.as_deref() != Some("pg_catalog") || posix.name != "POSIX" {
            return Err(format!("unexpected PostgreSQL POSIX identity: {posix:?}"));
        }

        let drift = diff_snapshots(&expected, &actual);
        if !drift.altered_objects.iter().any(|altered| {
            altered.object == "column parent_tenant"
                && altered.field == "collation"
                && altered.expected == "pg_catalog.C"
                && altered.actual == "pg_catalog.POSIX"
        }) {
            return Err(format!(
                "PostgreSQL collation change did not surface as drift: {drift:?}"
            ));
        }

        let live = LiveSchema::from_catalog_snapshot(actual, OWNER);
        let error = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
        .lower(&composite_fk_ir("postgres_collation_fk"), &live)
        .map(|_| ())
        .map_err(|error| error.to_string())
        .expect_err("different live PostgreSQL collations must reject a composite FK");
        if !error.contains("exact catalog collation differs")
            || !error.contains("pg_catalog.POSIX local")
            || !error.contains("pg_catalog.C target")
        {
            return Err(format!(
                "unexpected PostgreSQL collation diagnostic: {error}"
            ));
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("PostgreSQL collation regression failed: {work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL collation schema: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("PostgreSQL collation regression failed: {work}; cleanup also failed: {cleanup}")
        }
    }
}
