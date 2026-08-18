use crate::support;

use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::ir::CURRENT_IR_VERSION;
use zero_migrate::{
    AlterPrimaryKeyStep, Approval, ApprovalScope, IrAuthor, LiveSchema, MigrationEngine, PlanStep,
    SqlDialect, SqliteBackend,
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
    ExecutorConfig::new("pk-tests", "app", support::no_inject("app"))
}

fn step(name: &str, action: serde_json::Value) -> AlterPrimaryKeyStep {
    let ir: zero_migrate::MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": "app_test",
        "ops": [{
            "op": "alterPrimaryKey",
            "table": "items",
            "action": action
        }]
    }))
    .expect("primary-key IR parses");
    let plan = IrAuthor::new(
        "app",
        "app_test",
        SqlDialect::Sqlite,
        &support::no_inject("app"),
    )
    .lower_plan(&ir, &LiveSchema::default())
    .expect("primary-key IR lowers");
    match plan.steps.into_iter().next().expect("one step") {
        PlanStep::AlterPrimaryKey(step) => step,
        other => panic!("expected AlterPrimaryKey step, got {other:?}"),
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

async fn apply(backend: &SqliteBackend, step: &AlterPrimaryKeyStep) -> Result<bool, String> {
    MigrationBackend::alter_primary_key(
        backend,
        &cfg(),
        step,
        Approval::Approved,
        &ApprovalScope::All,
        "tester",
    )
    .await
    .map_err(|error| error.to_string())
}

async fn foreign_keys_enabled(backend: &SqliteBackend) -> bool {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    backend
        .actor()
        .query("PRAGMA foreign_keys")
        .await
        .expect("foreign_keys")
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.as_deref())
        == Some("1")
}

async fn primary_key(backend: &SqliteBackend) -> Vec<String> {
    let mut columns = backend
        .actor()
        .query("PRAGMA main.table_info('items')")
        .await
        .expect("table_info")
        .into_iter()
        .filter_map(|row| {
            let ordinal = row
                .get(5)
                .and_then(Clone::clone)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or_default();
            (ordinal > 0).then(|| (ordinal, row[1].clone().expect("column name")))
        })
        .collect::<Vec<_>>();
    columns.sort_by_key(|(ordinal, _)| *ordinal);
    columns.into_iter().map(|(_, name)| name).collect()
}

async fn column_is_not_null(backend: &SqliteBackend, name: &str) -> bool {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    backend
        .actor()
        .query("PRAGMA main.table_info('items')")
        .await
        .expect("table_info")
        .into_iter()
        .find(|row| row.get(1).and_then(Clone::clone).as_deref() == Some(name))
        .and_then(|row| row.get(3).and_then(Clone::clone))
        .as_deref()
        == Some("1")
}

#[compio::test]
async fn add_and_replace_both_directions_enforce_exact_expected_columns() {
    let paths = paths("pk_add_replace");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (code TEXT NOT NULL, payload TEXT, UNIQUE (code))",
    )
    .await;
    let add = step("add_pk", json!({"kind": "add", "columns": ["code"]}));
    let outcome = MigrationEngine::new()
        .apply_plan(
            &[PlanStep::AlterPrimaryKey(add.clone())],
            Approval::Approved,
            &backend,
            &cfg(),
            "tester",
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("apply_plan routes AlterPrimaryKey to the SQLite backend");
    assert!(outcome
        .applied
        .applied
        .contains(&add.migration.version.as_str().to_string()));
    assert_eq!(primary_key(&backend).await, ["code"]);
    assert!(foreign_keys_enabled(&backend).await);

    creator_exec(
        &backend,
        "ALTER TABLE items ADD COLUMN tenant TEXT NOT NULL DEFAULT 't'",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE UNIQUE INDEX items_tenant_code_uq ON items (tenant, code)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE INDEX items_payload_idx ON items (payload)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE TRIGGER items_touch AFTER UPDATE ON items BEGIN SELECT NEW.code; END",
    )
    .await;

    let mismatch = step(
        "replace_mismatch",
        json!({
            "kind": "replace",
            "expectedColumns": ["wrong"],
            "columns": ["tenant", "code"]
        }),
    );
    let error = apply(&backend, &mismatch)
        .await
        .expect_err("drift must refuse");
    assert!(
        error.contains("expected current primary key (wrong)"),
        "{error}"
    );
    assert_eq!(primary_key(&backend).await, ["code"]);

    let composite = step(
        "replace_composite",
        json!({
            "kind": "replace",
            "expectedColumns": ["code"],
            "columns": ["tenant", "code"]
        }),
    );
    assert!(apply(&backend, &composite).await.expect("replace succeeds"));
    assert_eq!(primary_key(&backend).await, ["tenant", "code"]);

    let reverse_mismatch = step(
        "replace_reverse_mismatch",
        json!({
            "kind": "replace",
            "expectedColumns": ["code", "tenant"],
            "columns": ["code"]
        }),
    );
    let error = apply(&backend, &reverse_mismatch)
        .await
        .expect_err("reordered composite drift must refuse");
    assert!(
        error.contains("expected current primary key (code, tenant)"),
        "{error}"
    );
    assert_eq!(primary_key(&backend).await, ["tenant", "code"]);

    let single = step(
        "replace_single",
        json!({
            "kind": "replace",
            "expectedColumns": ["tenant", "code"],
            "columns": ["code"]
        }),
    );
    assert!(apply(&backend, &single)
        .await
        .expect("reverse replace succeeds"));
    assert_eq!(primary_key(&backend).await, ["code"]);
    let dependents = backend
        .actor()
        .query(
            "SELECT type, name FROM main.sqlite_master \
             WHERE name IN ('items_payload_idx', 'items_touch') ORDER BY type, name",
        )
        .await
        .expect("dependents");
    assert_eq!(
        dependents.len(),
        2,
        "index and trigger survive both rebuilds"
    );
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn generated_integer_drop_requires_declaration_and_removes_sequence_state() {
    let paths = paths("pk_identity_drop");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT, UNIQUE (id))",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (payload) VALUES ('one')").await;

    let refused = step(
        "drop_without_identity",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    let error = apply(&backend, &refused)
        .await
        .expect_err("implicit generation transition must refuse");
    assert!(error.contains("dropIdentityFrom"), "{error}");
    assert_eq!(primary_key(&backend).await, ["id"]);

    let drop = step(
        "drop_with_identity",
        json!({
            "kind": "drop",
            "expectedColumns": ["id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    assert!(apply(&backend, &drop)
        .await
        .expect("declared drop succeeds"));
    assert!(primary_key(&backend).await.is_empty());
    let ddl = backend
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='items'")
        .await
        .expect("stored ddl")[0][0]
        .clone()
        .expect("ddl");
    assert!(!ddl.to_ascii_uppercase().contains("PRIMARY KEY"), "{ddl}");
    assert!(!ddl.to_ascii_uppercase().contains("AUTOINCREMENT"), "{ddl}");
    assert!(ddl.to_ascii_uppercase().contains("ID INTEGER"), "{ddl}");
    assert!(column_is_not_null(&backend, "id").await);
    let sequence = backend
        .actor()
        .query("SELECT seq FROM main.sqlite_sequence WHERE name='items'")
        .await
        .expect("sqlite_sequence");
    assert!(
        sequence.is_empty(),
        "removed generation has no sequence row"
    );
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn dropping_legacy_nullable_inline_primary_key_preserves_nullability_and_rows() {
    let paths = paths("pk_nullable_inline_drop");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (code TEXT PRIMARY KEY, payload TEXT)",
    )
    .await;
    creator_exec(
        &backend,
        "INSERT INTO items (code, payload) VALUES (NULL, 'legacy')",
    )
    .await;
    assert!(!column_is_not_null(&backend, "code").await);

    let drop = step(
        "drop_nullable_inline_pk",
        json!({"kind": "drop", "expectedColumns": ["code"]}),
    );
    assert!(apply(&backend, &drop)
        .await
        .expect("dropping a nullable legacy primary key succeeds"));
    assert!(primary_key(&backend).await.is_empty());
    assert!(!column_is_not_null(&backend, "code").await);
    let rows = backend
        .actor()
        .query("SELECT payload FROM items WHERE code IS NULL")
        .await
        .expect("legacy row survives");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some("legacy"));
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn table_level_integer_rowid_transition_materializes_not_null() {
    let paths = paths("pk_table_level_rowid");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (
           id INTEGER,
           tenant TEXT NOT NULL,
           PRIMARY KEY (id),
           UNIQUE (tenant, id)
         )",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (tenant) VALUES ('acme')").await;
    assert!(!column_is_not_null(&backend, "id").await);

    let replace = step(
        "replace_table_level_rowid",
        json!({
            "kind": "replace",
            "expectedColumns": ["id"],
            "columns": ["tenant", "id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    assert!(apply(&backend, &replace)
        .await
        .expect("table-level INTEGER PRIMARY KEY transition succeeds"));
    assert_eq!(primary_key(&backend).await, ["tenant", "id"]);
    assert!(column_is_not_null(&backend, "id").await);
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn quoted_keyword_column_is_not_misclassified_as_a_table_constraint() {
    let paths = paths("pk_quoted_keyword");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (\"primary\" TEXT NOT NULL UNIQUE, payload TEXT)",
    )
    .await;

    let add = step(
        "add_quoted_keyword_pk",
        json!({"kind": "add", "columns": ["primary"]}),
    );
    assert!(apply(&backend, &add)
        .await
        .expect("quoted keyword column remains a column during rewrite"));
    assert_eq!(primary_key(&backend).await, ["primary"]);
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn generated_integer_replace_to_composite_requires_and_applies_declared_transition() {
    let paths = paths("pk_identity_replace");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (
           'id' INTEGER PRIMARY KEY AUTOINCREMENT,
           tenant TEXT NOT NULL,
           UNIQUE (tenant, id)
         )",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (tenant) VALUES ('acme')").await;

    let refused = step(
        "replace_without_identity",
        json!({
            "kind": "replace",
            "expectedColumns": ["id"],
            "columns": ["tenant", "id"]
        }),
    );
    let error = apply(&backend, &refused)
        .await
        .expect_err("implicit rowid transition must refuse");
    assert!(error.contains("dropIdentityFrom"), "{error}");

    let replace = step(
        "replace_with_identity",
        json!({
            "kind": "replace",
            "expectedColumns": ["id"],
            "columns": ["tenant", "id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    assert!(apply(&backend, &replace)
        .await
        .expect("declared rowid transition succeeds"));
    assert_eq!(primary_key(&backend).await, ["tenant", "id"]);
    let ddl = backend
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='items'")
        .await
        .expect("stored ddl")[0][0]
        .clone()
        .expect("ddl");
    assert!(!ddl.to_ascii_uppercase().contains("AUTOINCREMENT"), "{ddl}");
    assert!(ddl.to_ascii_uppercase().contains("'ID' INTEGER"), "{ddl}");
    assert!(column_is_not_null(&backend, "id").await);
    let rows = backend
        .actor()
        .query("SELECT CAST(id AS TEXT), tenant FROM items ORDER BY id")
        .await
        .expect("copied rows");
    assert_eq!(
        rows,
        vec![vec![Some("1".to_string()), Some("acme".to_string())]],
        "the rebuild must copy the generated row and preserve its assigned id"
    );
    backend
        .actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator mode");
    backend
        .actor()
        .exec("INSERT INTO items (tenant) VALUES ('must-not-generate')")
        .await
        .expect_err("the former INTEGER PRIMARY KEY must no longer generate an id");
    let sequence = backend
        .actor()
        .query("SELECT seq FROM main.sqlite_sequence WHERE name='items'")
        .await
        .expect("sqlite_sequence");
    assert!(sequence.is_empty());
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn generated_integer_with_quoted_inline_constraint_name_rebuilds_cleanly() {
    let paths = paths("pk_quoted_inline_constraint");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (
           id INTEGER CONSTRAINT \"items primary key\" PRIMARY KEY AUTOINCREMENT,
           tenant TEXT NOT NULL,
           UNIQUE (tenant, id)
         )",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (tenant) VALUES ('acme')").await;

    let replace = step(
        "replace_quoted_inline_constraint",
        json!({
            "kind": "replace",
            "expectedColumns": ["id"],
            "columns": ["tenant", "id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    assert!(apply(&backend, &replace)
        .await
        .expect("quoted inline constraint name is removed with its PRIMARY KEY body"));
    assert_eq!(primary_key(&backend).await, ["tenant", "id"]);
    assert!(column_is_not_null(&backend, "id").await);
    assert_eq!(
        backend
            .actor()
            .query("SELECT CAST(id AS TEXT), tenant FROM items")
            .await
            .expect("copied row"),
        vec![vec![Some("1".to_string()), Some("acme".to_string())]]
    );
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn add_refuses_to_introduce_rowid_generation_and_nullable_targets() {
    let generation_paths = paths("pk_generation_add");
    let generation_backend = backend(&generation_paths);
    creator_exec(
        &generation_backend,
        "CREATE TABLE items (
           id INTEGER NOT NULL,
           nullable_code TEXT,
           UNIQUE (id),
           UNIQUE (nullable_code)
         )",
    )
    .await;

    let integer = step("add_integer", json!({"kind": "add", "columns": ["id"]}));
    let error = apply(&generation_backend, &integer)
        .await
        .expect_err("add cannot introduce generation");
    assert!(
        error.contains("would introduce SQLite INTEGER PRIMARY KEY"),
        "{error}"
    );

    let nullable = step(
        "add_nullable",
        json!({"kind": "add", "columns": ["nullable_code"]}),
    );
    let error = apply(&generation_backend, &nullable)
        .await
        .expect_err("target must already be not null");
    assert!(error.contains("must already be NOT NULL"), "{error}");
    assert!(primary_key(&generation_backend).await.is_empty());
    assert!(foreign_keys_enabled(&generation_backend).await);

    let legacy_paths = paths("pk_legacy_nullable_component");
    let legacy_backend = backend(&legacy_paths);
    creator_exec(
        &legacy_backend,
        "CREATE TABLE items (
           tenant TEXT,
           code TEXT,
           PRIMARY KEY (tenant, code),
           UNIQUE (tenant)
         )",
    )
    .await;
    creator_exec(
        &legacy_backend,
        "INSERT INTO items (tenant, code) VALUES (NULL, 'legacy')",
    )
    .await;
    let replace = step(
        "replace_nullable_pk_member",
        json!({
            "kind": "replace",
            "expectedColumns": ["tenant", "code"],
            "columns": ["tenant"]
        }),
    );
    let error = apply(&legacy_backend, &replace)
        .await
        .expect_err("legacy PK membership does not prove explicit NOT NULL");
    assert!(error.contains("must already be NOT NULL"), "{error}");
    assert_eq!(primary_key(&legacy_backend).await, ["tenant", "code"]);
    assert!(foreign_keys_enabled(&legacy_backend).await);
}

#[compio::test]
async fn rowid_detection_ignores_literals_and_excludes_integer_primary_key_desc() {
    let primary_paths = paths("pk_rowid_detection");
    let primary_backend = backend(&primary_paths);
    creator_exec(
        &primary_backend,
        "CREATE TABLE items (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           note TEXT DEFAULT 'WITHOUT ROWID',
           UNIQUE (id)
         )",
    )
    .await;
    let drop = step(
        "literal_spoof",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    let error = apply(&primary_backend, &drop)
        .await
        .expect_err("body literal cannot suppress rowid detection");
    assert!(error.contains("dropIdentityFrom"), "{error}");
    assert!(foreign_keys_enabled(&primary_backend).await);

    let desc_paths = paths("pk_desc_detection");
    let desc_backend = backend(&desc_paths);
    creator_exec(
        &desc_backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY DESC, UNIQUE (id))",
    )
    .await;
    let invalid_transition = step(
        "desc_not_generated",
        json!({
            "kind": "drop",
            "expectedColumns": ["id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    let error = apply(&desc_backend, &invalid_transition)
        .await
        .expect_err("PRIMARY KEY DESC is not a rowid generator");
    assert!(error.contains("is not the live generated"), "{error}");
    let plain_drop = step(
        "desc_plain_drop",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    assert!(apply(&desc_backend, &plain_drop)
        .await
        .expect("ordinary non-generated key drops without transition"));
    assert!(primary_key(&desc_backend).await.is_empty());
    assert!(foreign_keys_enabled(&desc_backend).await);
}

#[compio::test]
async fn implicit_inbound_fk_and_without_rowid_drop_refuse_deliberately() {
    let primary_paths = paths("pk_implicit_fk");
    let primary_backend = backend(&primary_paths);
    creator_exec(
        &primary_backend,
        "CREATE TABLE items (id TEXT NOT NULL PRIMARY KEY, UNIQUE (id))",
    )
    .await;
    creator_exec(
        &primary_backend,
        "CREATE TABLE child (item_id TEXT NOT NULL, FOREIGN KEY (item_id) REFERENCES items)",
    )
    .await;
    let drop = step(
        "implicit_fk_drop",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    let error = apply(&primary_backend, &drop)
        .await
        .expect_err("implicit reference cannot be migrated by PK lifecycle");
    assert!(error.contains("implicit inbound foreign key"), "{error}");
    assert_eq!(primary_key(&primary_backend).await, ["id"]);
    assert!(foreign_keys_enabled(&primary_backend).await);

    let without_paths = paths("pk_without_rowid");
    let without_backend = backend(&without_paths);
    creator_exec(
        &without_backend,
        "CREATE TABLE items (id TEXT NOT NULL PRIMARY KEY) WITHOUT ROWID",
    )
    .await;
    let drop = step(
        "without_rowid_drop",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    let error = apply(&without_backend, &drop)
        .await
        .expect_err("WITHOUT ROWID requires a primary key");
    assert!(
        error.contains("cannot drop the primary key from WITHOUT ROWID"),
        "{error}"
    );
    assert_eq!(primary_key(&without_backend).await, ["id"]);
    assert!(foreign_keys_enabled(&without_backend).await);
}

#[compio::test]
async fn candidate_unique_must_match_target_column_collation() {
    let paths = paths("pk_candidate_collation");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (code TEXT COLLATE NOCASE NOT NULL)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE UNIQUE INDEX items_code_binary_uq ON items (code COLLATE BINARY)",
    )
    .await;
    let add = step(
        "collation_candidate",
        json!({"kind": "add", "columns": ["code"]}),
    );
    let error = apply(&backend, &add)
        .await
        .expect_err("different-collation uniqueness is not an exact candidate");
    assert!(error.contains("exact pre-existing UNIQUE key"), "{error}");
    assert!(primary_key(&backend).await.is_empty());
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn same_named_table_and_column_cannot_spoof_candidate_collation() {
    let paths = paths("pk_same_name_collation");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (items TEXT COLLATE NOCASE NOT NULL)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE UNIQUE INDEX items_binary_uq ON items (items COLLATE BINARY)",
    )
    .await;
    let add = step(
        "same_name_collation_candidate",
        json!({"kind": "add", "columns": ["items"]}),
    );
    let error = apply(&backend, &add)
        .await
        .expect_err("table-name text cannot hide the column's NOCASE collation");
    assert!(error.contains("exact pre-existing UNIQUE key"), "{error}");
    assert!(primary_key(&backend).await.is_empty());
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn inbound_foreign_key_requires_exact_alternate_unique_key() {
    let paths = paths("pk_inbound");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id TEXT NOT NULL PRIMARY KEY, payload TEXT)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE TABLE child (id INTEGER PRIMARY KEY, item_id TEXT NOT NULL, \
         FOREIGN KEY (item_id) REFERENCES items (id))",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items VALUES ('a', 'payload')").await;
    creator_exec(&backend, "INSERT INTO child VALUES (1, 'a')").await;

    let drop = step(
        "drop_referenced_pk",
        json!({"kind": "drop", "expectedColumns": ["id"]}),
    );
    let error = apply(&backend, &drop)
        .await
        .expect_err("only referenced key must refuse");
    assert!(error.contains("inbound foreign key"), "{error}");

    creator_exec(
        &backend,
        "CREATE UNIQUE INDEX items_id_alternate_uq ON items (id)",
    )
    .await;
    assert!(apply(&backend, &drop)
        .await
        .expect("exact alternate unique permits drop"));
    assert!(primary_key(&backend).await.is_empty());
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn inbound_foreign_key_blocks_replace_until_an_exact_alternate_exists() {
    let paths = paths("pk_inbound_replace");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (
           id TEXT NOT NULL PRIMARY KEY,
           replacement_id TEXT NOT NULL UNIQUE,
           payload TEXT
         )",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE TABLE child (id INTEGER PRIMARY KEY, item_id TEXT NOT NULL, \
         FOREIGN KEY (item_id) REFERENCES items (id))",
    )
    .await;
    creator_exec(
        &backend,
        "INSERT INTO items VALUES ('a', 'new-a', 'payload')",
    )
    .await;
    creator_exec(&backend, "INSERT INTO child VALUES (1, 'a')").await;

    let replace = step(
        "replace_referenced_pk",
        json!({
            "kind": "replace",
            "expectedColumns": ["id"],
            "columns": ["replacement_id"]
        }),
    );
    let error = apply(&backend, &replace)
        .await
        .expect_err("replace must not remove the only referenced key");
    assert!(error.contains("inbound foreign key"), "{error}");
    assert_eq!(primary_key(&backend).await, ["id"]);

    creator_exec(
        &backend,
        "CREATE UNIQUE INDEX items_id_alternate_uq ON items (id)",
    )
    .await;
    assert!(apply(&backend, &replace)
        .await
        .expect("an exact alternate permits replacement"));
    assert_eq!(primary_key(&backend).await, ["replacement_id"]);
    creator_exec(
        &backend,
        "INSERT INTO items VALUES ('b', 'new-b', 'second')",
    )
    .await;
    creator_exec(&backend, "INSERT INTO child VALUES (2, 'b')").await;
    assert!(foreign_keys_enabled(&backend).await);
}

#[compio::test]
async fn foreign_key_check_abort_rolls_back_schema_sequence_and_dependents_and_restores_fk() {
    let paths = paths("pk_rollback");
    let backend = backend(&paths);
    creator_exec(
        &backend,
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT, UNIQUE (id))",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE TABLE child (id INTEGER PRIMARY KEY, item_id INTEGER NOT NULL, \
         FOREIGN KEY (item_id) REFERENCES items (id))",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE INDEX items_payload_idx ON items (payload)",
    )
    .await;
    creator_exec(
        &backend,
        "CREATE TRIGGER items_touch AFTER UPDATE ON items BEGIN SELECT NEW.id; END",
    )
    .await;
    creator_exec(&backend, "INSERT INTO items (payload) VALUES ('one')").await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    backend
        .actor()
        .exec("PRAGMA foreign_keys = OFF")
        .await
        .expect("disable for corruption fixture");
    creator_exec(&backend, "INSERT INTO child VALUES (1, 999)").await;
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    backend
        .actor()
        .exec("PRAGMA foreign_keys = ON")
        .await
        .expect("restore before operation");

    let drop = step(
        "drop_identity_rollback",
        json!({
            "kind": "drop",
            "expectedColumns": ["id"],
            "dropIdentityFrom": ["id"]
        }),
    );
    let error = apply(&backend, &drop)
        .await
        .expect_err("foreign_key_check must abort");
    assert!(error.contains("foreign_key_check"), "{error}");
    assert_eq!(primary_key(&backend).await, ["id"]);
    let ddl = backend
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='items'")
        .await
        .expect("ddl")[0][0]
        .clone()
        .expect("ddl");
    assert!(ddl.to_ascii_uppercase().contains("AUTOINCREMENT"), "{ddl}");
    let sequence = backend
        .actor()
        .query("SELECT seq FROM main.sqlite_sequence WHERE name='items'")
        .await
        .expect("sequence");
    assert_eq!(sequence[0][0].as_deref(), Some("1"));
    let dependents = backend
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master \
             WHERE name IN ('items_payload_idx', 'items_touch') ORDER BY name",
        )
        .await
        .expect("dependents");
    assert_eq!(dependents.len(), 2);
    let tmp = backend
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master \
             WHERE name='items__zero_migrate_rebuild'",
        )
        .await
        .expect("temp table");
    assert!(tmp.is_empty());
    assert!(foreign_keys_enabled(&backend).await);
    let journal = backend.applied_sqlite().await.expect("journal");
    assert!(!journal
        .iter()
        .any(|entry| entry.version == drop.migration.version.as_str()));
}
