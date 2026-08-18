//! Typed single-column reference lowering and compatibility regressions.
//!
//! These fixtures author only the column-level `IrColumn.references` shape. They
//! deliberately do not exercise the separate table-level composite-FK surface or
//! any lifecycle operation.

use crate::support;

use serde_json::{json, Value};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    fold_ops, snapshot_schema, validate_ir, ColumnSnapshot, ConstraintSnapshot, IrAuthor,
    LiveSchema, MysqlTextStorageSnapshot, SchemaSnapshot, SqlDialect, TableSnapshot,
    ValidatorDialect,
};

const PROJECT_SCHEMA: &str = "app";
const OWNER: &str = "app_typed_references";

fn no_inject_policy() -> zero_migrate::EffectivePolicy {
    support::no_inject(PROJECT_SCHEMA)
}

fn ir(name: &str, ops: Vec<Value>) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": ops,
    }))
    .expect("typed-reference fixture must deserialize")
}

fn column(
    name: &str,
    ty: &str,
    nullable: bool,
    value_format: Option<Value>,
    case_sensitive: Option<bool>,
    references: Option<Value>,
) -> Value {
    let mut column = json!({
        "name": name,
        "type": ty,
        "nullable": nullable,
    });
    let object = column
        .as_object_mut()
        .expect("column fixture is always an object");
    if let Some(value_format) = value_format {
        object.insert("valueFormat".to_string(), value_format);
    }
    if let Some(case_sensitive) = case_sensitive {
        object.insert("caseSensitive".to_string(), json!(case_sensitive));
    }
    if let Some(references) = references {
        object.insert("references".to_string(), references);
    }
    column
}

fn reference(table: &str, on_delete: Option<&str>, on_update: Option<&str>) -> Value {
    reference_column(table, "id", on_delete, on_update)
}

fn reference_column(
    table: &str,
    column: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
) -> Value {
    let mut reference = json!({
        "table": table,
        "column": column,
    });
    let object = reference
        .as_object_mut()
        .expect("reference fixture is always an object");
    if let Some(on_delete) = on_delete {
        object.insert("onDelete".to_string(), json!(on_delete));
    }
    if let Some(on_update) = on_update {
        object.insert("onUpdate".to_string(), json!(on_update));
    }
    reference
}

fn create_table(name: &str, columns: Vec<Value>, primary_key: Option<&[&str]>) -> Value {
    json!({
        "op": "createTable",
        "name": name,
        "columns": columns,
        "primaryKey": primary_key.map(|columns| columns.to_vec()),
        "indexes": [],
    })
}

fn type_id(prefix: &str) -> Value {
    json!({ "typeId": { "prefix": prefix } })
}

fn typed_reference_matrix_ir() -> MigrationIr {
    ir(
        "typed_reference_matrix",
        vec![
            create_table(
                "int_parents",
                vec![column("id", "int", false, None, None, None)],
                Some(&["id"]),
            ),
            create_table(
                "uuid_parents",
                vec![column("id", "uuid", false, None, None, None)],
                Some(&["id"]),
            ),
            create_table(
                "type_id_parents",
                vec![column(
                    "id",
                    "text",
                    false,
                    Some(type_id("account")),
                    None,
                    None,
                )],
                Some(&["id"]),
            ),
            create_table(
                "ulid_parents",
                vec![column("id", "text", false, Some(json!("ulid")), None, None)],
                Some(&["id"]),
            ),
            create_table(
                "children",
                vec![
                    column(
                        "int_parent_id",
                        "int",
                        true,
                        None,
                        None,
                        Some(reference("int_parents", None, None)),
                    ),
                    column(
                        "uuid_parent_id",
                        "uuid",
                        true,
                        None,
                        None,
                        Some(reference("uuid_parents", Some("cascade"), Some("setNull"))),
                    ),
                    column(
                        "type_id_parent_id",
                        "text",
                        true,
                        Some(type_id("account")),
                        None,
                        Some(reference(
                            "type_id_parents",
                            Some("cascade"),
                            Some("cascade"),
                        )),
                    ),
                    column(
                        "ulid_parent_id",
                        "text",
                        true,
                        Some(json!("ulid")),
                        None,
                        Some(reference("ulid_parents", Some("setNull"), Some("cascade"))),
                    ),
                ],
                None,
            ),
        ],
    )
}

fn create_marker(dialect: SqlDialect, table: &str) -> String {
    match dialect {
        SqlDialect::Postgres => format!("CREATE TABLE \"{PROJECT_SCHEMA}\".\"{table}\""),
        SqlDialect::Mysql => format!("CREATE TABLE `{PROJECT_SCHEMA}`.`{table}`"),
        SqlDialect::Sqlite => format!("CREATE TABLE \"{table}\""),
    }
}

fn assert_reference_target(sql: &str, dialect: SqlDialect, table: &str) {
    let matches = match dialect {
        SqlDialect::Postgres => {
            sql.contains(&format!("REFERENCES \"{PROJECT_SCHEMA}\".\"{table}\" (id)"))
        }
        SqlDialect::Mysql => {
            sql.contains(&format!("REFERENCES `{PROJECT_SCHEMA}`.`{table}` (`id`)"))
        }
        // SQLite foreign keys may not name a schema. Accept either canonical
        // identifier quoting style while refusing a project-schema qualifier.
        SqlDialect::Sqlite => {
            (sql.contains(&format!("REFERENCES \"{table}\" (id)"))
                || sql.contains(&format!("REFERENCES {table}(id)")))
                && !sql.contains(&format!("REFERENCES {PROJECT_SCHEMA}.{table}"))
        }
    };
    assert!(
        matches,
        "missing or invalid typed FK target {table:?} on {dialect:?}: {sql}"
    );
}

fn create_sql<'a>(
    migrations: &'a [zero_migrate::Migration],
    dialect: SqlDialect,
    table: &str,
) -> &'a str {
    let marker = create_marker(dialect, table);
    migrations
        .iter()
        .find(|migration| migration.up.starts_with(&marker))
        .unwrap_or_else(|| panic!("missing {marker} in {migrations:#?}"))
        .up
        .as_str()
}

#[test]
fn typed_integer_uuid_type_id_and_ulid_references_lower_on_every_dialect() {
    let ir = typed_reference_matrix_ir();

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        let migrations = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|error| panic!("{dialect:?} typed references must lower: {error}"));
        let child = create_sql(&migrations, dialect, "children");

        assert_eq!(
            child.matches("FOREIGN KEY").count(),
            4,
            "every explicitly typed column must retain its independent FK on {dialect:?}: {child}"
        );
        for target in [
            "int_parents",
            "uuid_parents",
            "type_id_parents",
            "ulid_parents",
        ] {
            assert_reference_target(child, dialect, target);
        }

        let expected_storage = match dialect {
            SqlDialect::Postgres => [
                "\"int_parent_id\" integer",
                "\"uuid_parent_id\" uuid",
                "\"type_id_parent_id\" text COLLATE \"C\"",
                "\"ulid_parent_id\" text COLLATE \"C\"",
            ],
            SqlDialect::Mysql => [
                "`int_parent_id` INT",
                "`uuid_parent_id` VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin",
                "`type_id_parent_id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin",
                "`ulid_parent_id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin",
            ],
            SqlDialect::Sqlite => [
                "\"int_parent_id\" INTEGER",
                "\"uuid_parent_id\" TEXT",
                "\"type_id_parent_id\" TEXT COLLATE BINARY",
                "\"ulid_parent_id\" TEXT COLLATE BINARY",
            ],
        };
        for storage in expected_storage {
            assert!(
                child.contains(storage),
                "typed local storage {storage:?} was not preserved on {dialect:?}: {child}"
            );
        }

        assert!(
            !child.contains("CHECK ("),
            "a typed reference must not duplicate a child format CHECK on {dialect:?}: {child}"
        );
        let uuid_parent = create_sql(&migrations, dialect, "uuid_parents");
        match dialect {
            SqlDialect::Postgres => assert!(
                !uuid_parent.contains("CHECK ("),
                "native PostgreSQL UUID storage needs no textual format CHECK: {uuid_parent}"
            ),
            SqlDialect::Mysql | SqlDialect::Sqlite => assert!(
                uuid_parent.contains("CHECK (") && uuid_parent.contains("0-9a-f"),
                "the authoritative UUID key must enforce canonical lowercase syntax on {dialect:?}: {uuid_parent}"
            ),
        }
        let type_id_parent = create_sql(&migrations, dialect, "type_id_parents");
        assert!(
            type_id_parent.contains("CHECK (") && type_id_parent.contains("account_"),
            "the authoritative TypeID key must retain its format CHECK on {dialect:?}: {type_id_parent}"
        );
        let ulid_parent = create_sql(&migrations, dialect, "ulid_parents");
        assert!(
            ulid_parent.contains("CHECK ("),
            "the authoritative ULID key must retain its format CHECK on {dialect:?}: {ulid_parent}"
        );
    }
}

#[test]
fn offline_fold_keeps_uuid_checks_on_keys_and_off_references() {
    let ir = typed_reference_matrix_ir();

    for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
        let snapshot = fold_ops(&ir.ops, dialect, PROJECT_SCHEMA, &support::no_inject("app"))
            .unwrap_or_else(|error| panic!("{dialect:?} typed references must fold: {error}"));
        let parent = snapshot.tables["uuid_parents"]
            .columns
            .iter()
            .find(|column| column.name == "id")
            .expect("UUID parent key is folded");
        assert_eq!(
            parent.inline_checks.len(),
            1,
            "authoritative UUID key must retain one canonical CHECK on {dialect:?}"
        );
        let child = snapshot.tables["children"]
            .columns
            .iter()
            .find(|column| column.name == "uuid_parent_id")
            .expect("UUID child reference is folded");
        assert!(
            child.inline_checks.is_empty(),
            "UUID references must not carry their own format CHECK on {dialect:?}"
        );
    }
}

#[test]
fn mysql_format_typed_references_accept_delete_and_update_actions_without_checks() {
    let ir = typed_reference_matrix_ir();
    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("MySQL format-typed references with actions must lower");
    let child = create_sql(&migrations, SqlDialect::Mysql, "children");

    assert!(
        child.contains("ON UPDATE SET NULL ON DELETE CASCADE"),
        "UUID-text reference lost an action: {child}"
    );
    assert!(
        child.contains("ON UPDATE CASCADE ON DELETE CASCADE"),
        "TypeID reference lost an action: {child}"
    );
    assert!(
        child.contains("ON UPDATE CASCADE ON DELETE SET NULL"),
        "ULID reference lost an action: {child}"
    );
    assert!(
        !child.contains("CHECK ("),
        "MySQL child format checks would conflict with referential actions: {child}"
    );
}

fn declared_reference_ir(
    name: &str,
    local_type: &str,
    local_format: Option<Value>,
    local_case_sensitive: Option<bool>,
    target_type: &str,
    target_format: Option<Value>,
    target_case_sensitive: Option<bool>,
) -> MigrationIr {
    // Put the child first to prove reference validation sees the complete
    // deterministic artifact graph rather than depending on operation order.
    ir(
        name,
        vec![
            create_table(
                "children",
                vec![column(
                    "parent_id",
                    local_type,
                    true,
                    local_format,
                    local_case_sensitive,
                    Some(reference("parents", None, None)),
                )],
                None,
            ),
            create_table(
                "parents",
                vec![column(
                    "id",
                    target_type,
                    false,
                    target_format,
                    target_case_sensitive,
                    None,
                )],
                Some(&["id"]),
            ),
        ],
    )
}

#[test]
fn sqlite_inlines_a_typed_reference_to_a_later_declared_parent() {
    let ir = declared_reference_ir(
        "sqlite_forward_typed_reference",
        "uuid",
        None,
        None,
        "uuid",
        None,
        None,
    );

    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Sqlite,
        &no_inject_policy(),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("SQLite must inline a logically proven forward reference");
    let child_position = migrations
        .iter()
        .position(|migration| migration.up.starts_with("CREATE TABLE \"children\""))
        .expect("child CREATE exists");
    let parent_position = migrations
        .iter()
        .position(|migration| migration.up.starts_with("CREATE TABLE \"parents\""))
        .expect("parent CREATE exists");
    assert!(
        child_position < parent_position,
        "the regression fixture must retain child-first operation order"
    );

    let child = create_sql(&migrations, SqlDialect::Sqlite, "children");
    assert_reference_target(child, SqlDialect::Sqlite, "parents");
}

fn assert_declared_mismatch(ir: &MigrationIr, expected: &[&str]) {
    for dialect in [
        ValidatorDialect::Postgres,
        ValidatorDialect::Mysql,
        ValidatorDialect::Sqlite,
    ] {
        let error = validate_ir(ir, dialect)
            .expect_err("each dialect must reject the declared reference mismatch");
        let rendered = error.to_string();
        for expected in expected {
            assert!(
                rendered.contains(expected),
                "{dialect:?} diagnostic must contain {expected:?}: {rendered}"
            );
        }
    }
}

fn reference_target_ir(name: &str, parent: Value) -> MigrationIr {
    ir(
        name,
        vec![
            create_table(
                "children",
                vec![column(
                    "parent_id",
                    "int",
                    true,
                    None,
                    None,
                    Some(reference("parents", None, None)),
                )],
                None,
            ),
            parent,
        ],
    )
}

#[test]
fn declared_reference_targets_must_be_single_column_keys() {
    let plain_parent = create_table(
        "parents",
        vec![column("id", "int", false, None, None, None)],
        None,
    );
    assert_declared_mismatch(
        &reference_target_ir("plain_non_key", plain_parent),
        &["not an eligible single-column primary or unique key"],
    );

    let composite_parent = create_table(
        "parents",
        vec![
            column("id", "int", false, None, None, None),
            column("tenant_id", "int", false, None, None, None),
        ],
        Some(&["id", "tenant_id"]),
    );
    assert_declared_mismatch(
        &reference_target_ir("composite_key_component", composite_parent),
        &["not an eligible single-column primary or unique key"],
    );

    let mut unique_column = column("id", "int", false, None, None, None);
    unique_column
        .as_object_mut()
        .expect("column fixture is an object")
        .insert("unique".to_string(), json!(true));
    let column_unique_parent = create_table("parents", vec![unique_column], None);
    let table_unique_parent = json!({
        "op": "createTable",
        "name": "parents",
        "columns": [column("id", "int", false, None, None, None)],
        "primaryKey": null,
        "constraints": [{ "kind": { "kind": "unique", "columns": ["id"] } }],
        "indexes": [],
    });
    let index_unique_parent = json!({
        "op": "createTable",
        "name": "parents",
        "columns": [column("id", "int", false, None, None, None)],
        "primaryKey": null,
        "constraints": [],
        "indexes": [{
            "columns": [{ "kind": "column", "name": "id" }],
            "unique": true,
        }],
    });
    for (name, parent) in [
        ("column_unique_key", column_unique_parent),
        ("unique_index_key", index_unique_parent),
    ] {
        let ir = reference_target_ir(name, parent);
        for dialect in [
            ValidatorDialect::Postgres,
            ValidatorDialect::Mysql,
            ValidatorDialect::Sqlite,
        ] {
            validate_ir(&ir, dialect).unwrap_or_else(|error| {
                panic!("{name} must be a valid reference key on {dialect:?}: {error}")
            });
        }
    }
    let ir = reference_target_ir("table_unique_key", table_unique_parent);
    for dialect in [ValidatorDialect::Postgres, ValidatorDialect::Mysql] {
        validate_ir(&ir, dialect).unwrap_or_else(|error| {
            panic!("table UNIQUE must be a valid reference key on {dialect:?}: {error}")
        });
    }
}

#[test]
fn explicit_non_id_reference_column_is_preserved() {
    let mut external_key = column("external_key", "text", false, None, None, None);
    external_key
        .as_object_mut()
        .expect("column fixture is an object")
        .insert("unique".to_string(), json!(true));
    let ir = ir(
        "explicit_reference_column",
        vec![
            create_table(
                "parents",
                vec![column("id", "int", false, None, None, None), external_key],
                Some(&["id"]),
            ),
            create_table(
                "children",
                vec![column(
                    "parent_key",
                    "text",
                    true,
                    None,
                    None,
                    Some(reference_column("parents", "external_key", None, None)),
                )],
                None,
            ),
        ],
    );

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        let migrations = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|error| panic!("{dialect:?} non-id reference must lower: {error}"));
        let child = create_sql(&migrations, dialect, "children");
        assert!(
            child.contains("external_key"),
            "the explicit target column was not rendered on {dialect:?}: {child}"
        );
        assert!(
            !child.contains("REFERENCES \"app\".\"parents\" (id)")
                && !child.contains("REFERENCES `app`.`parents` (`id`)")
                && !child.contains("REFERENCES parents(id)"),
            "the explicit target column silently fell back to id on {dialect:?}: {child}"
        );
    }
}

#[test]
fn declared_reference_graph_rejects_logical_width_format_prefix_and_collation_mismatches() {
    assert_declared_mismatch(
        &declared_reference_ir("int_width", "bigInt", None, None, "int", None, None),
        &[
            "logical integer width differs",
            "64-bit local",
            "32-bit target",
        ],
    );
    assert_declared_mismatch(
        &declared_reference_ir(
            "type_id_prefix",
            "text",
            Some(type_id("account")),
            None,
            "text",
            Some(type_id("acct")),
            None,
        ),
        &[
            "value formats differ",
            "TypeID(prefix=\"account\")",
            "TypeID(prefix=\"acct\")",
        ],
    );
    assert_declared_mismatch(
        &declared_reference_ir(
            "ulid_format",
            "text",
            Some(json!("ulid")),
            None,
            "text",
            Some(type_id("")),
            None,
        ),
        &["value formats differ", "ULID", "TypeID(prefix=\"\")"],
    );
    assert_declared_mismatch(
        &declared_reference_ir("collation", "text", None, None, "text", None, Some(false)),
        &[
            "collation intent differs",
            "caseSensitive=true local",
            "caseSensitive=false target",
        ],
    );
}

fn unmanaged_child_ir(name: &str, local_type: &str, local_format: Option<Value>) -> MigrationIr {
    ir(
        name,
        vec![create_table(
            "children",
            vec![column(
                "parent_id",
                local_type,
                true,
                local_format,
                None,
                Some(reference("unmanaged_parents", None, None)),
            )],
            None,
        )],
    )
}

fn unmanaged_live_with_case_sensitive(data_type: &str, case_sensitive: Option<bool>) -> LiveSchema {
    let mut snapshot = SchemaSnapshot::default();
    snapshot.tables.insert(
        "unmanaged_parents".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "id".to_string(),
                data_type: data_type.to_string(),
                nullable: false,
                case_sensitive,
                ..Default::default()
            }],
            indexes: Vec::new(),
            constraints: vec![ConstraintSnapshot {
                name: "unmanaged_parents_pkey".to_string(),
                kind: "PRIMARY KEY".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
                comment: None,
                cascade_columns: None,
            }],
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        },
    );
    LiveSchema::from_catalog_snapshot(snapshot, "external_owner")
}

fn unmanaged_live(data_type: &str) -> LiveSchema {
    unmanaged_live_with_case_sensitive(data_type, None)
}

fn live_pg_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let ordinal = NEXT.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    format!("{}_{}_{}", std::process::id(), nanos, ordinal)
}

#[test]
fn postgres_live_catalog_compares_formatted_reference_base_storage_separately_from_collation() {
    for (label, value_format) in [("type_id", type_id("account")), ("ulid", json!("ulid"))] {
        let parent = format!("{label}_parents");
        let target = ir(
            &format!("create_{parent}"),
            vec![create_table(
                &parent,
                vec![column(
                    "id",
                    "text",
                    false,
                    Some(value_format.clone()),
                    None,
                    None,
                )],
                Some(&["id"]),
            )],
        );
        let child_ir = ir(
            &format!("reference_{parent}"),
            vec![create_table(
                "children",
                vec![column(
                    "parent_id",
                    "text",
                    true,
                    Some(value_format),
                    None,
                    Some(reference(&parent, None, None)),
                )],
                None,
            )],
        );

        // This is the exact relevant shape recovered by PostgreSQL
        // introspection: data_type and format_type are both the base `text`;
        // the authored TypeID/ULID metadata separately supplies COLLATE "C".
        let mut snapshot = SchemaSnapshot::default();
        snapshot.tables.insert(
            parent.clone(),
            TableSnapshot {
                columns: vec![ColumnSnapshot {
                    name: "id".to_string(),
                    data_type: "text".to_string(),
                    nullable: false,
                    ddl_type_override: Some("text".to_string()),
                    ..Default::default()
                }],
                indexes: Vec::new(),
                constraints: vec![ConstraintSnapshot {
                    name: format!("{parent}_pkey"),
                    kind: "PRIMARY KEY".to_string(),
                    definition: "PRIMARY KEY (id)".to_string(),
                    comment: None,
                    cascade_columns: None,
                }],
                runtime_options: Default::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        );
        let mut live = LiveSchema::from_catalog_snapshot(snapshot, "external_owner");
        live.advance_logical_columns(&target, SqlDialect::Postgres, PROJECT_SCHEMA, None)
            .expect("record the authored formatted key contract");

        let migrations = IrAuthor::new(
            PROJECT_SCHEMA,
            OWNER,
            SqlDialect::Postgres,
            &no_inject_policy(),
        )
            .lower(&child_ir, &live)
            .unwrap_or_else(|error| {
                panic!(
                    "PostgreSQL live text storage must match a {label} reference whose DDL adds COLLATE C: {error}"
                )
            });
        let child = create_sql(&migrations, SqlDialect::Postgres, "children");
        assert!(
            child.contains(r#""parent_id" text COLLATE "C""#),
            "the explicit formatted reference storage was not preserved: {child}"
        );
        assert!(
            !child.contains("CHECK"),
            "a formatted child reference must not carry its own CHECK: {child}"
        );
        assert_reference_target(child, SqlDialect::Postgres, &parent);
    }
}

#[compio::test]
async fn live_postgres_introspection_validates_type_id_and_ulid_reference_storage() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("typed_refs_{}", live_pg_token());
    // Dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create isolated typed-reference schema");

    let result: Result<(String, usize), String> = async {
        let targets = ir(
            "live_pg_formatted_targets",
            vec![
                create_table(
                    "type_id_parents",
                    vec![column(
                        "id",
                        "text",
                        false,
                        Some(type_id("account")),
                        None,
                        None,
                    )],
                    Some(&["id"]),
                ),
                create_table(
                    "ulid_parents",
                    vec![column(
                        "id",
                        "text",
                        false,
                        Some(json!("ulid")),
                        None,
                        None,
                    )],
                    Some(&["id"]),
                ),
            ],
        );
        let parent_migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
            .lower(&targets, &LiveSchema::default())
            .map_err(|error| format!("lower formatted parent keys: {error}"))?;
        for migration in &parent_migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply formatted parent key: {error}"))?;
        }

        let parent_snapshot = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect formatted parent keys: {error}"))?;
        for table in ["type_id_parents", "ulid_parents"] {
            let column = parent_snapshot
                .tables
                .get(table)
                .and_then(|snapshot| snapshot.columns.iter().find(|column| column.name == "id"))
                .ok_or_else(|| format!("live snapshot omitted {table}.id"))?;
            if column.data_type != "text" || column.ddl_type_override.as_deref() != Some("text") {
                return Err(format!(
                    "PostgreSQL must introspect {table}.id as base text, got data_type={:?} ddl_type_override={:?}",
                    column.data_type, column.ddl_type_override
                ));
            }
        }

        let mut live = LiveSchema::from_catalog_snapshot(parent_snapshot, OWNER);
        live.advance_logical_columns(&targets, SqlDialect::Postgres, &schema, None)
            .map_err(|error| format!("record formatted parent contracts: {error}"))?;
        let children = ir(
            "live_pg_formatted_children",
            vec![create_table(
                "children",
                vec![
                    column(
                        "type_id_parent_id",
                        "text",
                        true,
                        Some(type_id("account")),
                        None,
                        Some(reference("type_id_parents", Some("cascade"), None)),
                    ),
                    column(
                        "ulid_parent_id",
                        "text",
                        true,
                        Some(json!("ulid")),
                        None,
                        Some(reference("ulid_parents", None, Some("cascade"))),
                    ),
                ],
                None,
            )],
        );
        let child_migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
            .lower(&children, &live)
            .map_err(|error| format!("lower typed child references from live catalog: {error}"))?;
        let marker = format!("CREATE TABLE \"{schema}\".\"children\"");
        let child_sql = child_migrations
            .iter()
            .find(|migration| migration.up.starts_with(&marker))
            .map(|migration| migration.up.clone())
            .ok_or_else(|| format!("missing live child CREATE TABLE in {child_migrations:#?}"))?;
        session
            .batch(&child_sql)
            .await
            .map_err(|error| format!("apply typed child references: {error}"))?;

        let applied = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect applied typed references: {error}"))?;
        let foreign_key_count = applied
            .tables
            .get("children")
            .ok_or_else(|| "live snapshot omitted children".to_string())?
            .constraints
            .iter()
            .filter(|constraint| constraint.kind.eq_ignore_ascii_case("FOREIGN KEY"))
            .count();
        Ok((child_sql, foreign_key_count))
    }
    .await;

    let cleanup = session
        .batch(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await;
    let (child_sql, foreign_key_count) = match (result, cleanup) {
        (Ok(value), Ok(())) => value,
        (Err(error), Ok(())) => panic!("live typed-reference regression failed: {error}"),
        (Ok(_), Err(error)) => panic!("drop isolated typed-reference schema: {error}"),
        (Err(work), Err(cleanup)) => {
            panic!("live typed-reference regression failed: {work}; cleanup also failed: {cleanup}")
        }
    };

    assert_eq!(
        child_sql.matches("text COLLATE \"C\"").count(),
        2,
        "both formatted references must retain bytewise collation: {child_sql}"
    );
    assert!(
        !child_sql.contains("CHECK"),
        "formatted references must not carry child format checks: {child_sql}"
    );
    assert_eq!(
        foreign_key_count, 2,
        "both typed references must exist in the live PostgreSQL catalog"
    );
}

#[test]
fn format_bearing_reference_to_unmanaged_target_without_authored_metadata_is_rejected() {
    for (name, local_type, local_format, expected_format) in [
        (
            "unmanaged_type_id_reference",
            "text",
            Some(type_id("account")),
            "TypeID(prefix=\"account\")",
        ),
        ("unmanaged_uuid_reference", "uuid", None, "canonical UUID"),
    ] {
        let ir = unmanaged_child_ir(name, local_type, local_format);
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
            let error = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
                .lower(&ir, &unmanaged_live("text"))
                .expect_err("a catalog type cannot supply missing authored format metadata");
            let rendered = error.to_string();
            assert!(
                rendered.contains("no authored value-format metadata"),
                "unexpected {dialect:?} unmanaged-format diagnostic: {rendered}"
            );
            assert!(
                rendered.contains(expected_format),
                "diagnostic must retain the recorded local format on {dialect:?}: {rendered}"
            );
        }
    }
}

#[test]
fn mysql_live_catalog_validates_but_does_not_select_declared_uuid_storage() {
    let ir = typed_reference_matrix_ir();
    let recorded = serde_json::to_value(&ir).expect("serialize declared UUID reference IR");
    assert_eq!(recorded["ops"][4]["columns"][1]["type"], "uuid");

    let mut snapshot = SchemaSnapshot::default();
    snapshot.tables.insert(
        "uuid_parents".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "id".to_string(),
                data_type: "varchar(36)".to_string(),
                nullable: false,
                mysql_text_storage: Some(MysqlTextStorageSnapshot {
                    character_set: "ascii".to_string(),
                    collation: "ascii_bin".to_string(),
                }),
                ..Default::default()
            }],
            constraints: vec![ConstraintSnapshot {
                name: "uuid_parents_pkey".to_string(),
                kind: "PRIMARY KEY".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
                comment: None,
                cascade_columns: None,
            }],
            indexes: Vec::new(),
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        },
    );
    let live = LiveSchema::from_catalog_snapshot(snapshot, "external_owner");
    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&ir, &live)
    .expect("live VARCHAR(36) must validate the recorded UUID reference contract");
    let child = create_sql(&migrations, SqlDialect::Mysql, "children");
    assert!(
        child.contains("`uuid_parent_id` VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin"),
        "the catalog must not replace explicit UUID storage: {child}"
    );
    assert_eq!(
        serde_json::to_value(&ir).expect("serialize after catalog validation")["ops"][4]["columns"]
            [1]["type"],
        "uuid"
    );

    let mut mismatched_live = live;
    mismatched_live
        .table_snapshots
        .get_mut("uuid_parents")
        .expect("UUID target snapshot")
        .columns[0]
        .mysql_text_storage = Some(MysqlTextStorageSnapshot {
        character_set: "utf8mb4".to_string(),
        collation: "utf8mb4_bin".to_string(),
    });
    let error = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&ir, &mismatched_live)
    .expect_err("binary collations on different MySQL character sets are incompatible");
    let rendered = error.to_string();
    assert!(
        rendered.contains("ascii / ascii_bin") && rendered.contains("utf8mb4 / utf8mb4_bin"),
        "exact MySQL character storage mismatch must be diagnostic: {rendered}"
    );
}

#[test]
fn primitive_unmanaged_reference_is_catalog_validated_without_type_inference() {
    let ir = unmanaged_child_ir("unmanaged_integer_reference", "int", None);
    let recorded = serde_json::to_value(&ir).expect("serialize primitive reference IR");
    assert_eq!(recorded["ops"][0]["columns"][0]["type"], "int");

    for (dialect, live_type, local_storage) in [
        (SqlDialect::Postgres, "integer", "\"parent_id\" integer"),
        (SqlDialect::Mysql, "int", "`parent_id` INT"),
        (SqlDialect::Sqlite, "integer", "\"parent_id\" INTEGER"),
    ] {
        let migrations = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &unmanaged_live(live_type))
            .unwrap_or_else(|error| {
                panic!("matching unmanaged primitive target must lower on {dialect:?}: {error}")
            });
        let child = create_sql(&migrations, dialect, "children");
        assert!(
            child.contains(local_storage),
            "the explicit local int storage was not preserved on {dialect:?}: {child}"
        );
        assert_reference_target(child, dialect, "unmanaged_parents");

        let error = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &unmanaged_live("text"))
            .expect_err("an incompatible live primitive target must be rejected");
        let rendered = error.to_string();
        assert!(
            rendered.to_ascii_lowercase().contains("catalog")
                && rendered.contains("unmanaged_parents.id"),
            "unexpected {dialect:?} live-type diagnostic: {rendered}"
        );

        let still_recorded = serde_json::to_value(&ir).expect("serialize IR after catalog checks");
        assert_eq!(
            still_recorded["ops"][0]["columns"][0]["type"], "int",
            "the live catalog must validate but never select the local storage"
        );
    }
}

#[test]
fn primitive_unmanaged_reference_requires_a_live_single_column_candidate_key() {
    let ir = unmanaged_child_ir("unmanaged_non_key_reference", "int", None);

    for (dialect, live_type) in [
        (SqlDialect::Postgres, "integer"),
        (SqlDialect::Mysql, "int"),
        (SqlDialect::Sqlite, "integer"),
    ] {
        let mut live = unmanaged_live(live_type);
        let target = live
            .table_snapshots
            .get_mut("unmanaged_parents")
            .expect("unmanaged target snapshot exists");
        target.constraints.clear();
        target.indexes.clear();

        let error = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &live)
            .expect_err("an ordinary live column is not independently referenceable");
        let rendered = error.to_string();
        assert!(
            rendered.contains("not an eligible single-column primary or unique key"),
            "unexpected {dialect:?} live candidate-key diagnostic: {rendered}"
        );
    }
}

#[test]
fn sqlite_unmanaged_integer_reference_keeps_declared_width() {
    for (local_type, live_type) in [("int", "bigint"), ("bigInt", "integer")] {
        let ir = unmanaged_child_ir("sqlite_integer_width_mismatch", local_type, None);
        let error = IrAuthor::new(
            PROJECT_SCHEMA,
            OWNER,
            SqlDialect::Sqlite,
            &no_inject_policy(),
        )
        .lower(&ir, &unmanaged_live(live_type))
        .expect_err("SQLite affinity must not erase an unmanaged key's integer width");
        let rendered = error.to_string();
        assert!(
            rendered.contains("local type") && rendered.contains("live target type"),
            "unexpected SQLite integer-width diagnostic: {rendered}"
        );
    }

    let bigint = unmanaged_child_ir("sqlite_bigint_width_match", "bigInt", None);
    IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Sqlite,
        &no_inject_policy(),
    )
    .lower(&bigint, &unmanaged_live("bigint"))
    .expect("an explicitly declared unmanaged BIGINT target matches local bigInt");
}

#[test]
fn sqlite_declared_bigint_reference_matches_managed_integer_storage() {
    let target = ir(
        "create_managed_parents",
        vec![create_table(
            "managed_parents",
            vec![column("id", "bigInt", false, None, None, None)],
            Some(&["id"]),
        )],
    );
    let child = ir(
        "reference_managed_parents",
        vec![create_table(
            "children",
            vec![column(
                "parent_id",
                "bigInt",
                true,
                None,
                None,
                Some(reference("managed_parents", None, None)),
            )],
            None,
        )],
    );

    // zero-migrate renders every managed SQLite integer width as INTEGER, so
    // PRAGMA reports this physical spelling even though the retained project
    // declaration remains bigInt.
    let mut snapshot = SchemaSnapshot::default();
    snapshot.tables.insert(
        "managed_parents".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "id".to_string(),
                data_type: "integer".to_string(),
                nullable: false,
                ..Default::default()
            }],
            constraints: vec![ConstraintSnapshot {
                name: "managed_parents_pkey".to_string(),
                kind: "PRIMARY KEY".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
                comment: None,
                cascade_columns: None,
            }],
            indexes: Vec::new(),
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        },
    );
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    live.advance_logical_columns(&target, SqlDialect::Sqlite, PROJECT_SCHEMA, None)
        .expect("record the managed BIGINT key contract");

    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Sqlite,
        &no_inject_policy(),
    )
    .lower(&child, &live)
    .expect("managed logical width must validate its engine-rendered INTEGER storage");
    let sql = create_sql(&migrations, SqlDialect::Sqlite, "children");
    assert!(sql.contains(r#""parent_id" INTEGER"#), "{sql}");
    assert_reference_target(sql, SqlDialect::Sqlite, "managed_parents");
}

#[test]
fn mysql_unmanaged_text_reference_validates_catalog_collation_intent() {
    let case_insensitive = ir(
        "mysql_case_insensitive_reference",
        vec![create_table(
            "children",
            vec![column(
                "parent_id",
                "text",
                true,
                None,
                Some(false),
                Some(reference("unmanaged_parents", None, None)),
            )],
            None,
        )],
    );

    IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(
        &case_insensitive,
        &unmanaged_live_with_case_sensitive("text", Some(false)),
    )
    .expect("matching MySQL case-insensitive catalog collation must validate");

    let error = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&case_insensitive, &unmanaged_live("text"))
    .expect_err("a binary MySQL target must not satisfy caseSensitive=false");
    assert!(
        error.to_string().contains("collation intent"),
        "unexpected MySQL collation diagnostic: {error}"
    );
}

/// One already-applied parent table as the live catalog reports it. The catalog
/// carries the physical column and its key objects but never the authored
/// value-format metadata, which is exactly why the contract must arrive
/// separately.
fn applied_parent_snapshot(
    table: &str,
    columns: Vec<ColumnSnapshot>,
    constraints: Vec<ConstraintSnapshot>,
) -> SchemaSnapshot {
    let mut snapshot = SchemaSnapshot::default();
    snapshot.tables.insert(
        table.to_string(),
        TableSnapshot {
            columns,
            indexes: Vec::new(),
            constraints,
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        },
    );
    snapshot
}

fn text_column(name: &str) -> ColumnSnapshot {
    ColumnSnapshot {
        name: name.to_string(),
        data_type: "text".to_string(),
        nullable: false,
        ddl_type_override: Some("text".to_string()),
        ..Default::default()
    }
}

fn contract_for<'a>(
    live: &'a LiveSchema,
    table: &str,
    column: &str,
) -> &'a zero_migrate::LogicalColumnContract {
    live.logical_columns
        .iter()
        .find(|(key, _)| key.table == table && key.column == column)
        .map(|(_, contract)| contract)
        .unwrap_or_else(|| panic!("absorbed contracts omitted {table}.{column}"))
}

#[test]
fn absorb_logical_columns_carries_an_applied_file_contract_into_a_later_foreign_key() {
    // Migration A is ALREADY APPLIED: it is folded into the catalog snapshot and
    // is never lowered again, so its authored TypeID contract can only reach
    // migration B through contract accumulation. A catalog cannot supply it.
    let applied = ir(
        "applied_create_accounts",
        vec![create_table(
            "accounts",
            vec![column(
                "id",
                "text",
                false,
                Some(type_id("account")),
                None,
                None,
            )],
            Some(&["id"]),
        )],
    );
    let appended = ir(
        "appended_create_sessions",
        vec![create_table(
            "sessions",
            vec![column(
                "account_id",
                "text",
                true,
                Some(type_id("account")),
                None,
                Some(reference("accounts", None, None)),
            )],
            None,
        )],
    );
    let snapshot = applied_parent_snapshot(
        "accounts",
        vec![text_column("id")],
        vec![ConstraintSnapshot {
            name: "accounts_pkey".to_string(),
            kind: "PRIMARY KEY".to_string(),
            definition: "PRIMARY KEY (id)".to_string(),
            comment: None,
            cascade_columns: None,
        }],
    );
    let author = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Postgres,
        &no_inject_policy(),
    );

    let error = author
        .lower(
            &appended,
            &LiveSchema::from_catalog_snapshot(snapshot.clone(), OWNER),
        )
        .expect_err("skipping the applied file's contracts must break the appended foreign key");
    assert!(
        error
            .to_string()
            .contains("no authored value-format metadata"),
        "unexpected missing-contract diagnostic: {error}"
    );

    let mut live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    live.absorb_logical_columns(&applied, SqlDialect::Postgres, PROJECT_SCHEMA, None)
        .expect("an applied artifact contributes contracts without strict lower-time validation");
    let migrations = author
        .lower(&appended, &live)
        .expect("the absorbed contract makes the appended foreign key lower");
    assert_reference_target(
        create_sql(&migrations, SqlDialect::Postgres, "sessions"),
        SqlDialect::Postgres,
        "accounts",
    );
}

#[test]
fn absorb_logical_columns_replays_the_candidate_key_lifecycle_of_an_applied_file() {
    // The referenced column is made referenceable only by a UNIQUE createIndex in
    // the SKIPPED file. An accumulator that replayed only the declaration-bearing
    // ops would keep the contract but lose that candidate key, and the appended
    // foreign key would be rejected as targeting a non-key column.
    let applied = ir(
        "applied_create_orgs",
        vec![
            create_table(
                "orgs",
                vec![
                    column("row_id", "bigInt", false, None, None, None),
                    column("public_id", "text", false, Some(type_id("org")), None, None),
                ],
                Some(&["row_id"]),
            ),
            json!({
                "op": "createIndex",
                "table": "orgs",
                "name": "orgs_public_id_key",
                "columns": [{ "kind": "column", "name": "public_id" }],
                "unique": true,
            }),
        ],
    );
    let appended = ir(
        "appended_create_org_links",
        vec![create_table(
            "org_links",
            vec![column(
                "org_public_id",
                "text",
                true,
                Some(type_id("org")),
                None,
                Some(reference_column("orgs", "public_id", None, None)),
            )],
            None,
        )],
    );
    let snapshot = applied_parent_snapshot(
        "orgs",
        vec![
            ColumnSnapshot {
                name: "row_id".to_string(),
                data_type: "bigint".to_string(),
                nullable: false,
                ..Default::default()
            },
            text_column("public_id"),
        ],
        vec![ConstraintSnapshot {
            name: "orgs_pkey".to_string(),
            kind: "PRIMARY KEY".to_string(),
            definition: "PRIMARY KEY (row_id)".to_string(),
            comment: None,
            cascade_columns: None,
        }],
    );

    let mut live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    live.absorb_logical_columns(&applied, SqlDialect::Postgres, PROJECT_SCHEMA, None)
        .expect("the applied artifact's UNIQUE index is part of its accumulated contracts");
    assert!(
        contract_for(&live, "orgs", "public_id").single_column_reference_key,
        "the UNIQUE createIndex in the applied file must survive accumulation"
    );

    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Postgres,
        &no_inject_policy(),
    )
    .lower(&appended, &live)
    .expect("a UNIQUE key declared by the applied file is a valid reference target");
    assert!(
        create_sql(&migrations, SqlDialect::Postgres, "org_links").contains(&format!(
            "REFERENCES \"{PROJECT_SCHEMA}\".\"orgs\" (\"public_id\")"
        )),
        "the appended foreign key did not target the absorbed UNIQUE key: {migrations:#?}"
    );
}

#[test]
fn absorb_logical_columns_accumulates_what_strict_advance_rejects() {
    // The applied file references a target authored in a file that is not in this
    // seed. Strict accumulation refuses it; the lenient path must still harvest
    // the file's own contracts, because refusing here would make an ordered replay
    // depend on seeding order it cannot control.
    let applied = ir(
        "applied_reference_to_an_unseeded_target",
        vec![create_table(
            "invoices",
            vec![
                column("id", "text", false, Some(type_id("invoice")), None, None),
                column(
                    "customer_id",
                    "text",
                    true,
                    Some(type_id("customer")),
                    None,
                    Some(reference("customers", None, None)),
                ),
            ],
            Some(&["id"]),
        )],
    );

    let error = LiveSchema::default()
        .advance_logical_columns(&applied, SqlDialect::Postgres, PROJECT_SCHEMA, None)
        .expect_err("strict accumulation still rejects an unseeded formatted target");
    assert!(
        error
            .to_string()
            .contains("no authored value-format metadata"),
        "unexpected strict diagnostic: {error}"
    );

    let mut lenient = LiveSchema::default();
    lenient
        .absorb_logical_columns(&applied, SqlDialect::Postgres, PROJECT_SCHEMA, None)
        .expect("the lenient path performs no lower-time reference validation");
    assert!(
        contract_for(&lenient, "invoices", "id").single_column_reference_key,
        "the absorbed primary key contract did not land"
    );
    assert!(
        contract_for(&lenient, "invoices", "customer_id")
            .value_format
            .is_some(),
        "the absorbed reference column contract did not land"
    );
}
