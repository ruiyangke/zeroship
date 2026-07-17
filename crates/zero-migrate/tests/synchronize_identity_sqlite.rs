use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::ir::CURRENT_IR_VERSION;
use zero_migrate::{
    IrAuthor, LiveSchema, PlanStep, SqlDialect, SqliteBackend, SynchronizeIdentityStep,
};

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(name: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    Paths {
        app: dir.path().join(format!("{name}.sqlite")),
        journal: dir.path().join(format!("{name}.migrations.sqlite")),
        _dir: dir,
    }
}

fn backend(paths: &Paths) -> SqliteBackend {
    SqliteBackend::open(&paths.app, &paths.journal).expect("open SQLite backend")
}

fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("identity-tests", "app")
}

fn step(name: &str, table: &str, column: &str) -> SynchronizeIdentityStep {
    let ir: zero_migrate::MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": "app_test",
        "ops": [{
            "op": "synchronizeIdentity",
            "table": table,
            "column": column,
            "writesQuiesced": "sqlite_identity_import_window"
        }]
    }))
    .expect("synchronizeIdentity IR parses");
    let plan = IrAuthor::new("app", "app_test", SqlDialect::Sqlite)
        .lower_plan(&ir, &LiveSchema::default())
        .expect("synchronizeIdentity IR lowers");
    match plan.steps.into_iter().next().expect("one step") {
        PlanStep::SynchronizeIdentity(step) => step,
        other => panic!("expected SynchronizeIdentity step, got {other:?}"),
    }
}

async fn creator_exec(backend: &SqliteBackend, sql: &str) {
    backend
        .actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");
    backend.actor().exec(sql).await.expect(sql);
}

async fn apply(backend: &SqliteBackend, step: &SynchronizeIdentityStep) -> Result<bool, String> {
    MigrationBackend::synchronize_identity(backend, &cfg(), step, "tester")
        .await
        .map_err(|error| error.to_string())
}

async fn scalar_i64(backend: &SqliteBackend, sql: &str) -> Option<i64> {
    backend
        .actor()
        .query(sql)
        .await
        .unwrap_or_else(|error| panic!("query {sql:?}: {error}"))
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.as_deref())
        .and_then(|value| value.parse().ok())
}

#[compio::test]
async fn autoincrement_behind_is_raised_to_live_max_and_next_is_non_colliding() {
    let paths = paths("identity_autoincrement_behind");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)",
    )
    .await;
    creator_exec(
        &backend,
        "INSERT INTO items (id, payload) VALUES (10, 'imported')",
    )
    .await;
    // sqlite_sequence tracks INSERTs, not a later UPDATE of the rowid alias.  This
    // creates a natural behind-generator state without fabricating catalog data.
    creator_exec(&backend, "UPDATE items SET id = 50 WHERE id = 10").await;
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT seq FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(10)
    );

    let sync = step("sync_items_behind", "items", "id");
    assert!(apply(&backend, &sync).await.expect("synchronize succeeds"));
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT seq FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(50)
    );

    creator_exec(&backend, "INSERT INTO items (payload) VALUES ('generated')").await;
    assert_eq!(
        scalar_i64(&backend, "SELECT MAX(id) FROM items").await,
        Some(51),
        "SQLite allocates seq + 1 after synchronization"
    );

    assert!(!apply(&backend, &sync)
        .await
        .expect("completed synchronization is an idempotent skip"));
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    let version = sync.migration.version.as_str().replace('\'', "''");
    let journal_rows = scalar_i64(
        &backend,
        &format!("SELECT count(*) FROM \"_mig\".schema_migrations WHERE version = '{version}'"),
    )
    .await;
    assert_eq!(
        journal_rows,
        Some(1),
        "the no-op retry writes no duplicate event"
    );
}

#[compio::test]
async fn autoincrement_already_ahead_never_moves_backward() {
    let paths = paths("identity_autoincrement_ahead");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (id) VALUES (100)").await;
    creator_exec(&backend, "DELETE FROM items WHERE id = 100").await;
    creator_exec(&backend, "INSERT INTO items (id) VALUES (10)").await;
    assert_eq!(
        scalar_i64(&backend, "SELECT MAX(id) FROM items").await,
        Some(10)
    );
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT seq FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(100)
    );

    let sync = step("sync_items_ahead", "items", "id");
    assert!(apply(&backend, &sync).await.expect("synchronize succeeds"));
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT seq FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(100),
        "an already-higher high-water mark is never reduced to the live max"
    );
    creator_exec(&backend, "INSERT INTO items (payload) VALUES ('generated')").await;
    assert_eq!(
        scalar_i64(&backend, "SELECT MAX(id) FROM items").await,
        Some(101)
    );
}

#[compio::test]
async fn autoincrement_missing_sequence_row_is_recreated_from_live_max() {
    let paths = paths("identity_autoincrement_missing_row");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (id) VALUES (70)").await;
    // sqlite_sequence is intentionally writable.  A missing row is therefore a
    // possible behind-generator state and must not make an AUTOINCREMENT column
    // look like ordinary rowid allocation.
    creator_exec(
        &backend,
        "DELETE FROM main.sqlite_sequence WHERE name = 'items'",
    )
    .await;
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT count(*) FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(0)
    );

    let sync = step("sync_items_missing_sequence", "items", "id");
    assert!(apply(&backend, &sync).await.expect("synchronize succeeds"));
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT seq FROM main.sqlite_sequence WHERE name = 'items'"
        )
        .await,
        Some(70)
    );
    creator_exec(&backend, "INSERT INTO items (payload) VALUES ('generated')").await;
    assert_eq!(
        scalar_i64(&backend, "SELECT MAX(id) FROM items").await,
        Some(71)
    );
}

#[compio::test]
async fn ordinary_integer_rowid_is_a_valid_noop_even_when_sqlite_sequence_exists() {
    let paths = paths("identity_ordinary_rowid");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE ordinary (id INTEGER PRIMARY KEY, payload TEXT)",
    )
    .await;
    // This other table creates main.sqlite_sequence.  Its existence must not make
    // the ordinary table look AUTOINCREMENT, and no fake row may be created for it.
    creator_exec(
        &backend,
        "CREATE TABLE generated (id INTEGER PRIMARY KEY AUTOINCREMENT)",
    )
    .await;
    creator_exec(
        &backend,
        "INSERT INTO ordinary (id, payload) VALUES (50, 'imported')",
    )
    .await;
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT count(*) FROM main.sqlite_sequence WHERE name = 'ordinary'"
        )
        .await,
        Some(0)
    );

    let sync = step("sync_ordinary", "ordinary", "id");
    assert!(apply(&backend, &sync)
        .await
        .expect("ordinary rowid synchronization is a validated no-op"));
    assert_eq!(
        scalar_i64(
            &backend,
            "SELECT count(*) FROM main.sqlite_sequence WHERE name = 'ordinary'"
        )
        .await,
        Some(0),
        "ordinary rowid allocation owns no sqlite_sequence row"
    );
    creator_exec(
        &backend,
        "INSERT INTO ordinary (payload) VALUES ('generated')",
    )
    .await;
    assert_eq!(
        scalar_i64(&backend, "SELECT MAX(id) FROM ordinary").await,
        Some(51),
        "ordinary rowid allocation derives its next value from live rows"
    );
}

#[compio::test]
async fn non_rowid_integer_shapes_are_rejected_before_journaling() {
    let paths = paths("identity_invalid_shapes");
    let backend = backend(&paths);
    for ddl in [
        "CREATE TABLE not_primary (id INTEGER, payload TEXT)",
        "CREATE TABLE int_spelling (id INT PRIMARY KEY, payload TEXT)",
        "CREATE TABLE descending (id INTEGER PRIMARY KEY DESC, payload TEXT)",
        "CREATE TABLE composite (id INTEGER, tenant INTEGER, PRIMARY KEY (id, tenant))",
        "CREATE TABLE no_rowid (id INTEGER PRIMARY KEY) WITHOUT ROWID",
    ] {
        creator_exec(&backend, ddl).await;
    }

    for table in [
        "not_primary",
        "int_spelling",
        "descending",
        "composite",
        "no_rowid",
    ] {
        let sync = step(&format!("sync_invalid_{table}"), table, "id");
        let error = apply(&backend, &sync)
            .await
            .expect_err("non-rowid identity shape must be rejected");
        assert!(
            error.contains("is not the generated INTEGER PRIMARY KEY rowid alias"),
            "clear rejection for {table}: {error}"
        );
        backend
            .actor()
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let version = sync.migration.version.as_str().replace('\'', "''");
        assert_eq!(
            scalar_i64(
                &backend,
                &format!(
                    "SELECT count(*) FROM \"_mig\".schema_migrations WHERE version = '{version}'"
                )
            )
            .await,
            Some(0),
            "failed validation for {table} must roll back without a completed event"
        );
    }
}
