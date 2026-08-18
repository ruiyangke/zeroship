//! Portable table-level composite foreign-key regressions.
//!
//! These tests intentionally author `createTable.constraints[].kind = "fk"`.
//! Column-level `references` is exercised separately at the bottom so the two
//! APIs cannot accidentally be coalesced into one relationship.

use crate::support;

use serde_json::{json, Value};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, fold_ops_onto, snapshot_schema, validate_ir, IrAuthor, LiveSchema,
    MysqlTextStorageSnapshot, PlanStep, RenameStep, SqlDialect, ValidatorDialect,
};

const PROJECT_SCHEMA: &str = "app";
const OWNER: &str = "app_composite_foreign_keys";

fn no_inject_policy() -> zero_migrate::EffectivePolicy {
    support::no_inject(PROJECT_SCHEMA)
}

fn column(
    name: &str,
    ty: &str,
    nullable: bool,
    value_format: Option<Value>,
    case_sensitive: Option<bool>,
) -> Value {
    let mut column = json!({
        "name": name,
        "type": ty,
        "nullable": nullable,
    });
    let object = column.as_object_mut().expect("column fixture is an object");
    if let Some(value_format) = value_format {
        object.insert("valueFormat".to_string(), value_format);
    }
    if let Some(case_sensitive) = case_sensitive {
        object.insert("caseSensitive".to_string(), json!(case_sensitive));
    }
    column
}

fn type_id(prefix: &str) -> Value {
    json!({ "typeId": { "prefix": prefix } })
}

fn index(name: &str, columns: &[&str], unique: bool) -> Value {
    json!({
        "name": name,
        "columns": columns
            .iter()
            .map(|name| json!({ "kind": "column", "name": name }))
            .collect::<Vec<_>>(),
        "unique": unique,
    })
}

fn table(
    name: &str,
    columns: Vec<Value>,
    primary_key: Option<&[&str]>,
    constraints: Vec<Value>,
    indexes: Vec<Value>,
) -> Value {
    json!({
        "op": "createTable",
        "name": name,
        "columns": columns,
        "primaryKey": primary_key.map(|columns| columns.to_vec()),
        "constraints": constraints,
        "indexes": indexes,
    })
}

fn composite_fk(local: &[&str], referenced: &[&str], actions: bool) -> Value {
    let mut kind = json!({
        "kind": "fk",
        "columns": local,
        "referencesTable": "parents",
        "referencesColumns": referenced,
    });
    if actions {
        let object = kind.as_object_mut().expect("FK kind fixture is an object");
        object.insert("onDelete".to_string(), json!("setNull"));
        object.insert("onUpdate".to_string(), json!("cascade"));
    }
    json!({
        "name": "children_parent_fk",
        "kind": kind,
    })
}

#[allow(clippy::too_many_arguments)]
fn fixture(
    name: &str,
    parent_columns: Vec<Value>,
    parent_primary_key: Option<&[&str]>,
    parent_indexes: Vec<Value>,
    child_columns: Vec<Value>,
    local_columns: &[&str],
    referenced_columns: &[&str],
    actions: bool,
) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": [
            table(
                "parents",
                parent_columns,
                parent_primary_key,
                vec![],
                parent_indexes,
            ),
            table(
                "children",
                child_columns,
                None,
                vec![composite_fk(local_columns, referenced_columns, actions)],
                vec![],
            ),
        ],
    }))
    .expect("composite-FK fixture must deserialize")
}

fn canonical_fixture(name: &str) -> MigrationIr {
    fixture(
        name,
        vec![
            column("tenant_id", "int", false, None, None),
            column("public_id", "text", false, Some(type_id("account")), None),
        ],
        Some(&["tenant_id", "public_id"]),
        vec![],
        vec![
            column("parent_tenant", "int", true, None, None),
            column(
                "parent_public_id",
                "text",
                true,
                Some(type_id("account")),
                None,
            ),
        ],
        &["parent_tenant", "parent_public_id"],
        &["tenant_id", "public_id"],
        true,
    )
}

fn create_sql<'a>(
    migrations: &'a [zero_migrate::Migration],
    dialect: SqlDialect,
    table: &str,
) -> &'a str {
    let markers = match dialect {
        SqlDialect::Postgres => vec![format!("CREATE TABLE \"{PROJECT_SCHEMA}\".\"{table}\"")],
        SqlDialect::Mysql => vec![format!("CREATE TABLE `{PROJECT_SCHEMA}`.`{table}`")],
        SqlDialect::Sqlite => vec![
            format!("CREATE TABLE \"{table}\""),
            format!("CREATE TABLE IF NOT EXISTS \"{table}\""),
        ],
    };
    migrations
        .iter()
        .find(|migration| {
            markers
                .iter()
                .any(|marker| migration.up.starts_with(marker))
        })
        .unwrap_or_else(|| panic!("missing one of {markers:?} in {migrations:#?}"))
        .up
        .as_str()
}

fn validator_dialect(dialect: SqlDialect) -> ValidatorDialect {
    match dialect {
        SqlDialect::Postgres => ValidatorDialect::Postgres,
        SqlDialect::Mysql => ValidatorDialect::Mysql,
        SqlDialect::Sqlite => ValidatorDialect::Sqlite,
    }
}

fn assert_tuple_order(sql: &str, dialect: SqlDialect) {
    let fk = &sql[sql
        .find("FOREIGN KEY")
        .unwrap_or_else(|| panic!("missing FOREIGN KEY on {dialect:?}: {sql}"))..];
    let local_first = fk.find("parent_tenant").expect("first local column");
    let local_second = fk.find("parent_public_id").expect("second local column");
    let references = fk.find("REFERENCES").expect("REFERENCES clause");
    let target = &fk[references..];
    let target_first = target.find("tenant_id").expect("first target column");
    let target_second = target.find("public_id").expect("second target column");
    assert!(
        local_first < local_second && local_second < references,
        "local tuple order changed on {dialect:?}: {fk}"
    );
    assert!(
        target_first < target_second,
        "referenced tuple order changed on {dialect:?}: {fk}"
    );
}

#[test]
fn create_time_composite_fk_lowers_inline_with_order_actions_and_supporting_index_everywhere() {
    let ir = canonical_fixture("portable_composite_fk");

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        let migrations = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|error| {
                panic!("create-time composite FK must lower on {dialect:?}: {error}")
            });
        let child = create_sql(&migrations, dialect, "children");

        assert_eq!(child.matches("FOREIGN KEY").count(), 1, "{child}");
        assert_tuple_order(child, dialect);
        assert!(child.contains("ON UPDATE CASCADE"), "{child}");
        assert!(child.contains("ON DELETE SET NULL"), "{child}");
        assert!(!child.contains("MATCH FULL"), "{child}");
        if dialect == SqlDialect::Sqlite {
            assert!(
                !child.contains(&format!("REFERENCES {PROJECT_SCHEMA}."))
                    && !child.contains(&format!("REFERENCES \"{PROJECT_SCHEMA}\".")),
                "SQLite must not schema-qualify a reference: {child}"
            );
        }

        let supporting_sql = if dialect == SqlDialect::Mysql {
            let key_start = child
                .find("KEY `children_parent_fk_idx`")
                .unwrap_or_else(|| {
                    panic!(
                        "MySQL must declare the planned index in CREATE TABLE before InnoDB can synthesize an implicit one: {child}"
                    )
                });
            assert!(
                key_start < child.find("FOREIGN KEY").expect("inline MySQL FK"),
                "the planned MySQL KEY must precede the inline FK in the preview: {child}"
            );
            &child[key_start..]
        } else {
            migrations
                .iter()
                .find(|migration| migration.name == "create_index_children_parent_fk_idx")
                .unwrap_or_else(|| {
                    panic!(
                        "supporting index must be an explicit preview unit on {dialect:?}: {migrations:#?}"
                    )
                })
                .up
                .as_str()
        };
        let first = supporting_sql
            .find("parent_tenant")
            .expect("first index column");
        let second = supporting_sql
            .find("parent_public_id")
            .expect("second index column");
        assert!(
            first < second,
            "supporting index must preserve FK order on {dialect:?}: {supporting_sql}"
        );

        let folded = fold_ops(&ir.ops, dialect, PROJECT_SCHEMA, &support::no_inject("app"))
            .unwrap_or_else(|error| panic!("composite FK must fold on {dialect:?}: {error}"));
        let index = folded.tables["children"]
            .indexes
            .iter()
            .find(|index| index.name == "children_parent_fk_idx")
            .expect("folded supporting index");
        assert_eq!(
            index.columns,
            vec!["parent_tenant".to_string(), "parent_public_id".to_string()],
            "fold must retain ordered supporting-index columns on {dialect:?}"
        );
    }
}

#[test]
fn an_exact_ordered_composite_unique_index_is_a_candidate_key_on_every_dialect() {
    let ir = fixture(
        "composite_unique_candidate",
        vec![
            column("tenant_id", "int", false, None, None),
            column("external_id", "int", false, None, None),
        ],
        None,
        vec![index(
            "parents_tenant_external_key",
            &["tenant_id", "external_id"],
            true,
        )],
        vec![
            column("parent_tenant", "int", true, None, None),
            column("parent_external", "int", true, None, None),
        ],
        &["parent_tenant", "parent_external"],
        &["tenant_id", "external_id"],
        false,
    );

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        validate_ir(&ir, validator_dialect(dialect)).unwrap_or_else(|error| {
            panic!("ordered unique candidate must validate on {dialect:?}: {error}")
        });
        IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|error| {
                panic!("ordered unique candidate must lower on {dialect:?}: {error}")
            });
    }
}

fn lifecycle_fk_constraint() -> Value {
    json!({
        "name": "children_parent_fk",
        "kind": {
            "kind": "fk",
            "columns": ["parent_tenant", "parent_code"],
            "referencesTable": "parents",
            "referencesColumns": ["tenant_id", "code"]
        }
    })
}

fn lifecycle_tables(parent_constraints: Vec<Value>, parent_indexes: Vec<Value>) -> Vec<Value> {
    vec![
        table(
            "parents",
            vec![
                column("tenant_id", "int", false, None, None),
                column("code", "int", false, None, None),
            ],
            None,
            parent_constraints,
            parent_indexes,
        ),
        table(
            "children",
            vec![
                column("parent_tenant", "int", true, None, None),
                column("parent_code", "int", true, None, None),
            ],
            None,
            vec![],
            vec![],
        ),
    ]
}

fn lifecycle_ir(name: &str, ops: Vec<Value>) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": ops,
    }))
    .expect("candidate-key lifecycle fixture deserializes")
}

#[test]
fn ordered_unique_creation_is_visible_to_a_later_composite_fk_on_every_dialect() {
    let mut index_ops = lifecycle_tables(vec![], vec![]);
    index_ops.push(json!({
        "op": "createIndex",
        "table": "parents",
        "name": "parents_tenant_code_key",
        "columns": [
            { "kind": "column", "name": "tenant_id" },
            { "kind": "column", "name": "code" }
        ],
        "unique": true
    }));
    index_ops.push(json!({
        "op": "addConstraint",
        "table": "children",
        "constraint": lifecycle_fk_constraint()
    }));
    let index_ir = lifecycle_ir("ordered_unique_index_then_fk", index_ops);
    let base_ir = lifecycle_ir(
        "ordered_unique_index_then_fk_base",
        lifecycle_tables(vec![], vec![]),
    );
    let index_delta_ir = lifecycle_ir(
        "ordered_unique_index_then_fk_delta",
        vec![
            json!({
                "op": "createIndex",
                "table": "parents",
                "name": "parents_tenant_code_key",
                "columns": [
                    { "kind": "column", "name": "tenant_id" },
                    { "kind": "column", "name": "code" }
                ],
                "unique": true
            }),
            json!({
                "op": "addConstraint",
                "table": "children",
                "constraint": lifecycle_fk_constraint()
            }),
        ],
    );

    let mut constraint_ops = lifecycle_tables(vec![], vec![]);
    constraint_ops.push(json!({
        "op": "addConstraint",
        "table": "parents",
        "constraint": {
            "name": "parents_tenant_code_unique",
            "kind": {
                "kind": "unique",
                "columns": ["tenant_id", "code"]
            }
        }
    }));
    constraint_ops.push(json!({
        "op": "addConstraint",
        "table": "children",
        "constraint": lifecycle_fk_constraint()
    }));
    let constraint_ir = lifecycle_ir("ordered_unique_constraint_then_fk", constraint_ops);

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        validate_ir(&index_ir, validator_dialect(dialect)).unwrap_or_else(|error| {
            panic!("create unique index then FK must validate on {dialect:?}: {error}")
        });
        IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower_steps(&index_ir, &LiveSchema::default())
            .unwrap_or_else(|error| {
                panic!("create unique index then FK must lower on {dialect:?}: {error}")
            });

        // Exercise the physical precheck too: both tables already exist in the
        // input snapshot without the key, so the later FK is valid only if the
        // preceding createIndex is replayed in artifact order.
        let base_snapshot = fold_ops(
            &base_ir.ops,
            dialect,
            PROJECT_SCHEMA,
            &support::no_inject("app"),
        )
        .unwrap_or_else(|error| panic!("base schema folds on {dialect:?}: {error}"));
        let mut live = LiveSchema::from_catalog_snapshot(base_snapshot, OWNER);
        live.advance_logical_columns(&base_ir, dialect, PROJECT_SCHEMA, None)
            .unwrap_or_else(|error| panic!("base logical schema advances on {dialect:?}: {error}"));
        IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower_steps(&index_delta_ir, &live)
            .unwrap_or_else(|error| {
                panic!("ordered unique-index delta then FK must lower on {dialect:?}: {error}")
            });

        // SQLite has no native ALTER TABLE ADD UNIQUE lifecycle operation. This
        // block used to assert that SQLite nonetheless CLEARED validate, and then
        // skipped lower on SQLite alone - the gate accepting work the lowerer
        // rejects, which is the exact defect `dialect-support.toml` now records by
        // declaring `addConstraint/unique` `unsupported` there. Validate and lower
        // agree from here: both refuse. The candidate-key replay this test is about
        // keeps its executable all-target coverage from the unique-INDEX artifact
        // above, which IS portable on SQLite.
        if dialect == SqlDialect::Sqlite {
            let error = validate_ir(&constraint_ir, validator_dialect(dialect))
                .expect_err("addConstraint(unique) is not authorable on SQLite");
            assert!(
                error
                    .to_string()
                    .contains("SQLite cannot add or drop a table constraint in place"),
                "SQLite must refuse addConstraint(unique) as an unsupported op, not for \
                 some other reason: {error}"
            );
        } else {
            validate_ir(&constraint_ir, validator_dialect(dialect)).unwrap_or_else(|error| {
                panic!("add UNIQUE constraint then FK must validate on {dialect:?}: {error}")
            });
            IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
                .lower_steps(&constraint_ir, &LiveSchema::default())
                .unwrap_or_else(|error| {
                    panic!("add UNIQUE constraint then FK must lower on {dialect:?}: {error}")
                });
        }
    }
}

#[test]
fn dropping_the_only_ordered_candidate_key_before_a_composite_fk_is_rejected_everywhere() {
    let mut index_ops = lifecycle_tables(
        vec![],
        vec![index(
            "parents_tenant_code_key",
            &["tenant_id", "code"],
            true,
        )],
    );
    index_ops.push(json!({
        "op": "dropIndex",
        "table": "parents",
        "name": "parents_tenant_code_key",
        "unique": true
    }));
    index_ops.push(json!({
        "op": "addConstraint",
        "table": "children",
        "constraint": lifecycle_fk_constraint()
    }));
    let index_ir = lifecycle_ir("drop_unique_index_then_fk", index_ops);

    let mut constraint_ops = lifecycle_tables(vec![], vec![]);
    constraint_ops.push(json!({
        "op": "addConstraint",
        "table": "parents",
        "constraint": {
            "name": "parents_tenant_code_unique",
            "kind": {
                "kind": "unique",
                "columns": ["tenant_id", "code"]
            }
        }
    }));
    constraint_ops.push(json!({
        "op": "dropConstraint",
        "table": "parents",
        "name": "parents_tenant_code_unique"
    }));
    constraint_ops.push(json!({
        "op": "addConstraint",
        "table": "children",
        "constraint": lifecycle_fk_constraint()
    }));
    let constraint_ir = lifecycle_ir("drop_unique_constraint_then_fk", constraint_ops);

    for dialect in [
        ValidatorDialect::Postgres,
        ValidatorDialect::Mysql,
        ValidatorDialect::Sqlite,
    ] {
        // The UNIQUE-CONSTRAINT artifact is PostgreSQL/MySQL-only. `addConstraint`
        // of a unique constraint is declared `unsupported` on SQLite (there is no
        // in-place ADD CONSTRAINT), so on SQLite that IR is refused at its FIRST op
        // and never reaches the candidate-key check this test exists to pin. The
        // unique-INDEX artifact is portable on all three and carries SQLite here.
        let artifacts: Vec<(&str, &MigrationIr)> = if matches!(dialect, ValidatorDialect::Sqlite) {
            vec![("drop unique index", &index_ir)]
        } else {
            vec![
                ("drop unique index", &index_ir),
                ("drop UNIQUE constraint", &constraint_ir),
            ]
        };
        for (label, ir) in artifacts {
            let error = validate_ir(ir, dialect)
                .expect_err("dropping the only candidate key before its FK must fail");
            assert!(
                error
                    .to_string()
                    .contains("not backed by an exact PRIMARY KEY or UNIQUE"),
                "unexpected {label} diagnostic on {dialect:?}: {error}"
            );
        }
    }
}

#[test]
fn mysql_composite_fk_add_and_drop_are_native_and_never_disable_checks() {
    let base: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_composite_fk_base",
        "owner_app": OWNER,
        "ops": [
            table(
                "parents",
                vec![
                    column("tenant_id", "int", false, None, None),
                    column("public_id", "int", false, None, None),
                ],
                Some(&["tenant_id", "public_id"]),
                vec![],
                vec![],
            ),
            table(
                "children",
                vec![
                    column("parent_tenant", "int", true, None, None),
                    column("parent_public_id", "int", true, None, None),
                ],
                None,
                vec![],
                vec![],
            ),
        ],
    }))
    .expect("base fixture deserializes");
    let live_snapshot = fold_ops(
        &base.ops,
        SqlDialect::Mysql,
        PROJECT_SCHEMA,
        &support::no_inject("app"),
    )
    .expect("base schema folds");
    let live = LiveSchema::from_catalog_snapshot(live_snapshot.clone(), OWNER);
    let add: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_add_composite_fk",
        "owner_app": OWNER,
        "ops": [{
            "op": "addConstraint",
            "table": "children",
            "constraint": composite_fk(
                &["parent_tenant", "parent_public_id"],
                &["tenant_id", "public_id"],
                true,
            )
        }],
    }))
    .expect("add fixture deserializes");
    let added = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&add, &live)
    .expect("MySQL composite FK add lowers");
    assert_eq!(added.len(), 2, "supporting index precedes constraint add");
    assert!(added[0].up.starts_with("CREATE INDEX"), "{}", added[0].up);
    assert!(
        added[1]
            .up
            .contains("ADD CONSTRAINT `children_parent_fk` FOREIGN KEY")
            && added[1]
                .up
                .contains("(`parent_tenant`, `parent_public_id`)"),
        "{}",
        added[1].up
    );
    assert!(
        added.iter().all(|migration| {
            !migration
                .up
                .to_ascii_lowercase()
                .contains("foreign_key_checks")
                && migration
                    .down
                    .as_deref()
                    .is_none_or(|down| !down.to_ascii_lowercase().contains("foreign_key_checks"))
        }),
        "MySQL FK alteration must never toggle foreign_key_checks: {added:#?}"
    );
    let folded_after = fold_ops_onto(
        &live_snapshot,
        &add.ops,
        SqlDialect::Mysql,
        PROJECT_SCHEMA,
        &support::no_inject("app"),
    )
    .expect("stand-alone composite FK folds onto the live shape");
    let folded_support = folded_after.tables["children"]
        .indexes
        .iter()
        .find(|index| index.name == "children_parent_fk_idx")
        .expect("fold must retain the explicitly planned supporting index");
    assert_eq!(
        folded_support.columns,
        vec!["parent_tenant".to_string(), "parent_public_id".to_string()]
    );

    let declared = canonical_fixture("mysql_drop_composite_fk_base");
    let declared_snapshot = fold_ops(
        &declared.ops,
        SqlDialect::Mysql,
        PROJECT_SCHEMA,
        &support::no_inject("app"),
    )
    .expect("declared FK schema folds");
    let declared_live = LiveSchema::from_catalog_snapshot(declared_snapshot, OWNER);
    let drop: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_drop_composite_fk",
        "owner_app": OWNER,
        "ops": [{
            "op": "dropConstraint",
            "table": "children",
            "name": "children_parent_fk"
        }],
    }))
    .expect("drop fixture deserializes");
    let dropped = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&drop, &declared_live)
    .expect("MySQL composite FK drop lowers");
    assert_eq!(dropped.len(), 1);
    assert_eq!(
        dropped[0].up,
        "ALTER TABLE `app`.`children` DROP FOREIGN KEY `children_parent_fk`"
    );
    assert!(!dropped[0]
        .up
        .to_ascii_lowercase()
        .contains("foreign_key_checks"));
}

#[test]
fn mysql_composite_fk_compares_exact_live_character_storage_per_position() {
    let base: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_composite_fk_exact_text_storage_base",
        "owner_app": OWNER,
        "ops": [
            table(
                "parents",
                vec![
                    column("tenant_id", "int", false, None, None),
                    column("code", "text", false, None, Some(true)),
                ],
                Some(&["tenant_id", "code"]),
                vec![],
                vec![],
            ),
            table(
                "children",
                vec![
                    column("parent_tenant", "int", true, None, None),
                    column("parent_code", "text", true, None, Some(true)),
                ],
                None,
                vec![],
                vec![],
            ),
        ],
    }))
    .expect("base fixture deserializes");
    let mut snapshot = fold_ops(
        &base.ops,
        SqlDialect::Mysql,
        PROJECT_SCHEMA,
        &support::no_inject("app"),
    )
    .expect("base schema folds");
    snapshot
        .tables
        .get_mut("parents")
        .unwrap()
        .columns
        .iter_mut()
        .find(|column| column.name == "code")
        .unwrap()
        .mysql_text_storage = Some(MysqlTextStorageSnapshot {
        character_set: "utf8mb4".to_string(),
        collation: "utf8mb4_bin".to_string(),
    });
    snapshot
        .tables
        .get_mut("children")
        .unwrap()
        .columns
        .iter_mut()
        .find(|column| column.name == "parent_code")
        .unwrap()
        .mysql_text_storage = Some(MysqlTextStorageSnapshot {
        character_set: "ascii".to_string(),
        collation: "ascii_bin".to_string(),
    });
    let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    let add: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_composite_fk_exact_text_storage_add",
        "owner_app": OWNER,
        "ops": [{
            "op": "addConstraint",
            "table": "children",
            "constraint": {
                "name": "children_parent_fk",
                "kind": {
                    "kind": "fk",
                    "columns": ["parent_tenant", "parent_code"],
                    "referencesTable": "parents",
                    "referencesColumns": ["tenant_id", "code"]
                }
            }
        }]
    }))
    .expect("add fixture deserializes");
    let error = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Mysql,
        &no_inject_policy(),
    )
    .lower(&add, &live)
    .expect_err("different exact live MySQL collations must be rejected");
    assert!(
        error
            .to_string()
            .contains("MySQL character storage differs"),
        "unexpected exact-storage diagnostic: {error}"
    );
}

#[test]
fn sqlite_drop_then_add_change_uses_the_prior_rebuild_shape() {
    let declared = canonical_fixture("sqlite_changed_fk_base");
    let live_snapshot = fold_ops(
        &declared.ops,
        SqlDialect::Sqlite,
        PROJECT_SCHEMA,
        &support::no_inject("app"),
    )
    .expect("declared SQLite schema folds");
    let live = LiveSchema::from_catalog_snapshot(live_snapshot, OWNER);
    let changed: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "sqlite_change_composite_fk_name",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "dropConstraint",
                "table": "children",
                "name": "children_parent_fk"
            },
            {
                "op": "addConstraint",
                "table": "children",
                "constraint": {
                    "name": "children_parent_v2_fk",
                    "kind": {
                        "kind": "fk",
                        "columns": ["parent_tenant", "parent_public_id"],
                        "referencesTable": "parents",
                        "referencesColumns": ["tenant_id", "public_id"],
                        "onDelete": "cascade"
                    }
                }
            }
        ]
    }))
    .expect("SQLite change fixture deserializes");
    let steps = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Sqlite,
        &no_inject_policy(),
    )
    .lower_steps(&changed, &live)
    .expect("SQLite drop+add change lowers to rebuilds");
    assert_eq!(steps.len(), 2);
    let PlanStep::OnlineRename(RenameStep::SqliteRebuild(second)) = &steps[1] else {
        panic!("second FK change must be a rebuild: {steps:#?}");
    };
    assert!(second
        .spec
        .new_table_create
        .contains("children_parent_v2_fk"));
    assert!(
        !second
            .spec
            .new_table_create
            .contains("CONSTRAINT \"children_parent_fk\""),
        "the second rebuild must not resurrect the FK dropped by the first: {}",
        second.spec.new_table_create
    );
}

fn assert_rejected_on_every_dialect(label: &str, ir: &MigrationIr) {
    for dialect in [
        ValidatorDialect::Postgres,
        ValidatorDialect::Mysql,
        ValidatorDialect::Sqlite,
    ] {
        let Err(error) = validate_ir(ir, dialect) else {
            panic!("{label} must be rejected on {dialect:?}");
        };
        assert!(
            error
                .to_string()
                .to_ascii_lowercase()
                .contains("foreign key"),
            "{label} should produce an FK-specific diagnostic on {dialect:?}: {error}"
        );
    }
}

#[test]
fn rejects_empty_unequal_and_duplicate_tuples_on_every_dialect() {
    let base_parent = || {
        vec![
            column("a", "int", false, None, None),
            column("b", "int", false, None, None),
        ]
    };
    let base_child = || {
        vec![
            column("x", "int", true, None, None),
            column("y", "int", true, None, None),
        ]
    };
    for (label, local, referenced) in [
        ("empty tuple", vec![], vec![]),
        ("unequal arity", vec!["x", "y"], vec!["a"]),
        ("duplicate local", vec!["x", "x"], vec!["a", "b"]),
        ("duplicate referenced", vec!["x", "y"], vec!["a", "a"]),
    ] {
        let ir = fixture(
            label,
            base_parent(),
            Some(&["a", "b"]),
            vec![],
            base_child(),
            &local,
            &referenced,
            false,
        );
        assert_rejected_on_every_dialect(label, &ir);
    }
}

#[test]
fn rejects_missing_local_and_referenced_columns_on_every_dialect() {
    for (label, local, referenced) in [
        ("missing local column", ["missing", "y"], ["a", "b"]),
        ("missing referenced column", ["x", "y"], ["a", "missing"]),
    ] {
        let ir = fixture(
            label,
            vec![
                column("a", "int", false, None, None),
                column("b", "int", false, None, None),
            ],
            Some(&["a", "b"]),
            vec![],
            vec![
                column("x", "int", true, None, None),
                column("y", "int", true, None, None),
            ],
            &local,
            &referenced,
            false,
        );
        assert_rejected_on_every_dialect(label, &ir);
    }
}

#[test]
fn rejects_per_position_type_integer_width_format_and_collation_mismatches() {
    let cases = [
        (
            "logical type",
            column("child_value", "text", true, None, None),
            column("value", "uuid", false, None, None),
        ),
        (
            "integer width",
            column("child_value", "bigInt", true, None, None),
            column("value", "int", false, None, None),
        ),
        (
            "TypeID prefix",
            column("child_value", "text", true, Some(type_id("account")), None),
            column("value", "text", false, Some(type_id("workspace")), None),
        ),
        (
            "ULID format",
            column("child_value", "text", true, Some(json!("ulid")), None),
            column("value", "text", false, None, None),
        ),
        (
            "collation",
            column("child_value", "text", true, None, Some(true)),
            column("value", "text", false, None, Some(false)),
        ),
    ];

    for (label, child_value, parent_value) in cases {
        let ir = fixture(
            label,
            vec![column("tenant_id", "int", false, None, None), parent_value],
            Some(&["tenant_id", "value"]),
            vec![],
            vec![
                column("parent_tenant", "int", true, None, None),
                child_value,
            ],
            &["parent_tenant", "child_value"],
            &["tenant_id", "value"],
            false,
        );
        assert_rejected_on_every_dialect(label, &ir);
    }
}

#[test]
fn rejects_non_candidate_partial_and_reordered_target_tuples() {
    let plain_parent = fixture(
        "not a candidate key",
        vec![
            column("a", "int", false, None, None),
            column("b", "int", false, None, None),
        ],
        None,
        vec![],
        vec![
            column("x", "int", true, None, None),
            column("y", "int", true, None, None),
        ],
        &["x", "y"],
        &["a", "b"],
        false,
    );
    assert_rejected_on_every_dialect("not a candidate key", &plain_parent);

    let partial = fixture(
        "partial candidate key",
        vec![
            column("a", "int", false, None, None),
            column("b", "int", false, None, None),
            column("c", "int", false, None, None),
        ],
        Some(&["a", "b", "c"]),
        vec![],
        vec![
            column("x", "int", true, None, None),
            column("y", "int", true, None, None),
        ],
        &["x", "y"],
        &["a", "b"],
        false,
    );
    assert_rejected_on_every_dialect("partial candidate key", &partial);

    let reordered = fixture(
        "reordered candidate key",
        vec![
            column("a", "int", false, None, None),
            column("b", "int", false, None, None),
        ],
        Some(&["a", "b"]),
        vec![],
        vec![
            column("x", "int", true, None, None),
            column("y", "int", true, None, None),
        ],
        &["x", "y"],
        &["b", "a"],
        false,
    );
    assert_rejected_on_every_dialect("reordered candidate key", &reordered);
}

#[test]
fn sqlite_observes_match_simple_for_partially_null_local_tuples() {
    let ir = canonical_fixture("sqlite_match_simple");
    let migrations = IrAuthor::new(
        PROJECT_SCHEMA,
        OWNER,
        SqlDialect::Sqlite,
        &no_inject_policy(),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("SQLite composite FK must lower");
    let connection = rusqlite::Connection::open_in_memory().expect("open SQLite");
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .expect("enable SQLite foreign keys");
    for migration in &migrations {
        connection
            .execute_batch(&migration.up)
            .unwrap_or_else(|error| panic!("apply {}: {error}\n{}", migration.name, migration.up));
    }

    let valid_type_id = "account_00000000000000000000000000";
    connection
        .execute(
            "INSERT INTO children(parent_tenant, parent_public_id) VALUES(NULL, ?1)",
            [valid_type_id],
        )
        .expect("MATCH SIMPLE permits a null first component");
    connection
        .execute(
            "INSERT INTO children(parent_tenant, parent_public_id) VALUES(7, NULL)",
            [],
        )
        .expect("MATCH SIMPLE permits a null second component");
    assert!(
        connection
            .execute(
                "INSERT INTO children(parent_tenant, parent_public_id) VALUES(7, ?1)",
                [valid_type_id],
            )
            .is_err(),
        "a fully non-null tuple must require a parent match"
    );
    connection
        .execute(
            "INSERT INTO parents(tenant_id, public_id) VALUES(7, ?1)",
            [valid_type_id],
        )
        .expect("insert matching parent");
    connection
        .execute(
            "INSERT INTO children(parent_tenant, parent_public_id) VALUES(7, ?1)",
            [valid_type_id],
        )
        .expect("a fully matching tuple is accepted");
}

fn column_reference(table: &str) -> Value {
    json!({ "table": table, "column": "id" })
}

#[test]
fn repeated_column_level_references_remain_independent_single_column_constraints() {
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "independent_column_references",
        "owner_app": OWNER,
        "ops": [
            table(
                "left_parents",
                vec![column("id", "int", false, None, None)],
                Some(&["id"]),
                vec![],
                vec![],
            ),
            table(
                "right_parents",
                vec![column("id", "int", false, None, None)],
                Some(&["id"]),
                vec![],
                vec![],
            ),
            table(
                "children",
                vec![
                    {
                        let mut value = column("left_id", "int", true, None, None);
                        value.as_object_mut().expect("column object").insert(
                            "references".to_string(),
                            column_reference("left_parents"),
                        );
                        value
                    },
                    {
                        let mut value = column("right_id", "int", true, None, None);
                        value.as_object_mut().expect("column object").insert(
                            "references".to_string(),
                            column_reference("right_parents"),
                        );
                        value
                    },
                ],
                None,
                vec![],
                vec![],
            ),
        ],
    }))
    .expect("column-reference fixture must deserialize");

    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        let migrations = IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
            .lower(&ir, &LiveSchema::default())
            .unwrap_or_else(|error| panic!("column references lower on {dialect:?}: {error}"));
        let child = create_sql(&migrations, dialect, "children");
        assert_eq!(
            child.matches("FOREIGN KEY").count(),
            2,
            "column references must remain two constraints on {dialect:?}: {child}"
        );
        assert!(child.contains("left_parents"), "{child}");
        assert!(child.contains("right_parents"), "{child}");
        let fk_tail = &child[child.find("FOREIGN KEY").expect("first FK")..];
        assert!(
            !fk_tail.contains("left_id, right_id")
                && !fk_tail.contains("\"left_id\", \"right_id\"")
                && !fk_tail.contains("`left_id`, `right_id`"),
            "independent references were coalesced on {dialect:?}: {child}"
        );
    }
}

fn live_pg_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "composite_fk_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

#[compio::test]
async fn live_postgres_composite_fk_introspection_and_policy_drift() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = live_pg_token();
    // Dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create isolated composite-FK schema");

    let result: Result<(), String> = async {
        let ir = canonical_fixture("live_pg_composite_fk");
        let migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower composite FK: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        let expected = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect composite FK: {error}"))?;
        let foreign_key = expected
            .tables
            .get("children")
            .and_then(|table| {
                table
                    .constraints
                    .iter()
                    .find(|constraint| constraint.name == "children_parent_fk")
            })
            .ok_or_else(|| "live PG snapshot omitted children_parent_fk".to_string())?;
        let expected_definition = format!(
            "FOREIGN KEY (parent_tenant, parent_public_id) REFERENCES \
             {schema}.parents(tenant_id, public_id) ON UPDATE CASCADE ON DELETE SET NULL"
        );
        if foreign_key.definition != expected_definition {
            return Err(format!(
                "unexpected live composite FK definition: {:?}",
                foreign_key.definition
            ));
        }

        session
            .batch(&format!(
                "ALTER TABLE \"{schema}\".\"children\" \
                   DROP CONSTRAINT \"children_parent_fk\"; \
                 ALTER TABLE \"{schema}\".\"children\" \
                   ADD CONSTRAINT \"children_parent_fk\" \
                   FOREIGN KEY (\"parent_tenant\", \"parent_public_id\") \
                   REFERENCES \"{schema}\".\"parents\" (\"tenant_id\", \"public_id\") \
                   ON DELETE CASCADE ON UPDATE RESTRICT"
            ))
            .await
            .map_err(|error| format!("mutate composite FK policy: {error}"))?;
        let actual = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("re-introspect changed composite FK: {error}"))?;
        let drift = diff_snapshots(&expected, &actual);
        if !drift.altered_objects.iter().any(|altered| {
            altered.object == "constraint children_parent_fk"
                && altered.field == "definition"
                && altered.expected.contains("ON UPDATE CASCADE")
                && altered.actual.contains("ON UPDATE RESTRICT")
        }) {
            return Err(format!(
                "PG composite-FK policy change did not surface as drift: {drift:?}"
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
        (Err(work), Ok(())) => panic!("live composite-FK regression failed: {work}"),
        (Ok(()), Err(cleanup)) => panic!("drop composite-FK schema: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("live composite-FK regression failed: {work}; cleanup failed: {cleanup}")
        }
    }
}
