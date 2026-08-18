//! Catalog-proved value formats for references into an unmanaged live target.
//!
//! A format-bearing local column may reference a target that has no authored
//! contract when the live catalog carries the target's own format evidence:
//! PostgreSQL's native `uuid` type, the engine's exact UUID spelling CHECK on
//! MySQL/SQLite, or a recovered TypeID/ULID CHECK on any dialect. A target that
//! omits its own CHECK and inherits safety through its foreign key carries no
//! such evidence and stays rejected.
//!
//! Both reference surfaces are exercised: the column-level `IrColumn.references`
//! facet and the table-level single-column `constraints[].kind = "fk"` facet.
//! They run through separate validation loops, so each needs its own coverage.

use crate::support;

use serde_json::{json, Value};
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    ColumnSnapshot, ConstraintSnapshot, IrAuthor, LiveSchema, MysqlTextStorageSnapshot,
    SchemaSnapshot, SqlDialect, TableSnapshot, ValueFormat,
};

const PROJECT_SCHEMA: &str = "app";
const OWNER: &str = "app_catalog_format_proof";
const PARENT: &str = "unmanaged_parents";
const MISSING_METADATA: &str = "no authored value-format metadata";

fn no_inject_policy() -> zero_migrate::EffectivePolicy {
    support::no_inject(PROJECT_SCHEMA)
}

/// The format evidence an unmanaged live target column carries in the catalog.
#[derive(Clone, Copy, Debug)]
enum Evidence {
    /// The engine's own canonical UUID contract: PostgreSQL's native `uuid`
    /// type, or the exact UUID spelling CHECK on MySQL/SQLite.
    Uuid,
    /// UUID storage with no local CHECK: the shape of a chained typed reference
    /// on MySQL/SQLite. PostgreSQL has no such shape because its native `uuid`
    /// type is itself the contract.
    UuidWithoutCheck,
    /// A recovered TypeID CHECK with this exact prefix.
    TypeId(&'static str),
    /// TypeID storage with no local CHECK: a chained typed reference.
    TypeIdWithoutCheck,
    /// Generic unmanaged text with no format contract of any kind.
    PlainText,
}

fn ascii_bin() -> MysqlTextStorageSnapshot {
    MysqlTextStorageSnapshot {
        character_set: "ascii".to_string(),
        collation: "ascii_bin".to_string(),
    }
}

fn target_column(dialect: SqlDialect, evidence: Evidence) -> ColumnSnapshot {
    let mut column = ColumnSnapshot {
        name: "id".to_string(),
        nullable: false,
        ..Default::default()
    };
    match evidence {
        Evidence::Uuid | Evidence::UuidWithoutCheck => match dialect {
            SqlDialect::Postgres => column.data_type = "uuid".to_string(),
            SqlDialect::Mysql => {
                column.data_type = "varchar(36)".to_string();
                column.mysql_text_storage = Some(ascii_bin());
                column.catalog_uuid_format_check = matches!(evidence, Evidence::Uuid);
            }
            SqlDialect::Sqlite => {
                column.data_type = "text".to_string();
                column.catalog_uuid_format_check = matches!(evidence, Evidence::Uuid);
            }
        },
        Evidence::TypeId(_) | Evidence::TypeIdWithoutCheck => {
            match dialect {
                SqlDialect::Postgres | SqlDialect::Sqlite => {
                    column.data_type = "text".to_string();
                }
                SqlDialect::Mysql => {
                    column.data_type = "varchar(191)".to_string();
                    column.mysql_text_storage = Some(ascii_bin());
                }
            }
            if let Evidence::TypeId(prefix) = evidence {
                column.value_format = Some(ValueFormat::TypeId {
                    prefix: prefix.to_string(),
                });
            }
        }
        Evidence::PlainText => match dialect {
            SqlDialect::Postgres | SqlDialect::Sqlite => {
                column.data_type = "text".to_string();
            }
            SqlDialect::Mysql => column.data_type = "varchar(191)".to_string(),
        },
    }
    column
}

fn live(dialect: SqlDialect, evidence: Evidence) -> LiveSchema {
    let mut snapshot = SchemaSnapshot::default();
    snapshot.tables.insert(
        PARENT.to_string(),
        TableSnapshot {
            columns: vec![target_column(dialect, evidence)],
            indexes: Vec::new(),
            constraints: vec![ConstraintSnapshot {
                name: format!("{PARENT}_pkey"),
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

fn type_id(prefix: &str) -> Value {
    json!({ "typeId": { "prefix": prefix } })
}

fn local_column(ty: &str, value_format: Option<Value>, references: Option<Value>) -> Value {
    let mut column = json!({
        "name": "parent_id",
        "type": ty,
        "nullable": true,
    });
    let object = column.as_object_mut().expect("column fixture is an object");
    if let Some(value_format) = value_format {
        object.insert("valueFormat".to_string(), value_format);
    }
    if let Some(references) = references {
        object.insert("references".to_string(), references);
    }
    column
}

fn ir(name: &str, columns: Vec<Value>, constraints: Vec<Value>) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": "children",
            "columns": columns,
            "primaryKey": null,
            "constraints": constraints,
            "indexes": [],
        }],
    }))
    .expect("catalog-format-proof fixture must deserialize")
}

/// The column-level `references` surface.
fn column_reference_ir(name: &str, ty: &str, value_format: Option<Value>) -> MigrationIr {
    ir(
        name,
        vec![local_column(
            ty,
            value_format,
            Some(json!({ "table": PARENT, "column": "id" })),
        )],
        Vec::new(),
    )
}

/// The table-level single-column foreign-key surface.
fn table_constraint_ir(name: &str, ty: &str, value_format: Option<Value>) -> MigrationIr {
    ir(
        name,
        vec![local_column(ty, value_format, None)],
        vec![json!({
            "name": "children_parent_fk",
            "kind": {
                "kind": "fk",
                "columns": ["parent_id"],
                "referencesTable": PARENT,
                "referencesColumns": ["id"],
            },
        })],
    )
}

fn lower(ir: &MigrationIr, dialect: SqlDialect, live: &LiveSchema) -> Result<(), String> {
    IrAuthor::new(PROJECT_SCHEMA, OWNER, dialect, &no_inject_policy())
        .lower(ir, live)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

const DIALECTS: [SqlDialect; 3] = [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite];

#[test]
fn catalog_uuid_evidence_proves_a_table_level_single_column_foreign_key() {
    let ir = table_constraint_ir("table_level_uuid_fk", "uuid", None);
    for dialect in DIALECTS {
        lower(&ir, dialect, &live(dialect, Evidence::Uuid)).unwrap_or_else(|error| {
            panic!("live UUID evidence must prove the {dialect:?} table-level FK: {error}")
        });
    }
}

#[test]
fn catalog_uuid_evidence_proves_a_column_level_reference() {
    let ir = column_reference_ir("column_level_uuid_reference", "uuid", None);
    for dialect in DIALECTS {
        lower(&ir, dialect, &live(dialect, Evidence::Uuid)).unwrap_or_else(|error| {
            panic!("live UUID evidence must prove the {dialect:?} column reference: {error}")
        });
    }
}

#[test]
fn exactly_matching_catalog_type_id_evidence_proves_both_reference_surfaces() {
    let table_level = table_constraint_ir("table_level_type_id_fk", "text", Some(type_id("acct")));
    let column_level = column_reference_ir(
        "column_level_type_id_reference",
        "text",
        Some(type_id("acct")),
    );
    for dialect in DIALECTS {
        let live = live(dialect, Evidence::TypeId("acct"));
        lower(&table_level, dialect, &live).unwrap_or_else(|error| {
            panic!("matching TypeID evidence must prove the {dialect:?} table-level FK: {error}")
        });
        lower(&column_level, dialect, &live).unwrap_or_else(|error| {
            panic!("matching TypeID evidence must prove the {dialect:?} column reference: {error}")
        });
    }
}

#[test]
fn differing_catalog_type_id_evidence_stays_rejected() {
    let table_level = table_constraint_ir(
        "table_level_type_id_mismatch",
        "text",
        Some(type_id("acct")),
    );
    let column_level = column_reference_ir(
        "column_level_type_id_mismatch",
        "text",
        Some(type_id("acct")),
    );
    for dialect in DIALECTS {
        let live = live(dialect, Evidence::TypeId("order"));
        for (surface, ir) in [
            ("table-level", &table_level),
            ("column-level", &column_level),
        ] {
            let error = lower(ir, dialect, &live)
                .expect_err("a different catalog TypeID prefix must not prove this reference");
            assert!(
                error.contains(MISSING_METADATA),
                "{surface} {dialect:?} prefix mismatch must stay a missing-metadata rejection: {error}"
            );
        }
    }
}

#[test]
fn a_live_text_target_without_uuid_evidence_stays_rejected() {
    let table_level = table_constraint_ir("table_level_uuid_no_evidence", "uuid", None);
    let column_level = column_reference_ir("column_level_uuid_no_evidence", "uuid", None);
    for dialect in DIALECTS {
        let live = live(dialect, Evidence::PlainText);
        for (surface, ir) in [
            ("table-level", &table_level),
            ("column-level", &column_level),
        ] {
            let error = lower(ir, dialect, &live)
                .expect_err("plain unmanaged text cannot prove a canonical UUID contract");
            assert!(
                error.contains(MISSING_METADATA),
                "{surface} {dialect:?} plain-text target must stay rejected: {error}"
            );
        }
    }
}

#[test]
fn a_chained_typed_reference_target_without_its_own_check_stays_rejected() {
    let table_level = table_constraint_ir("table_level_chained", "text", Some(type_id("acct")));
    let column_level = column_reference_ir("column_level_chained", "text", Some(type_id("acct")));
    for dialect in DIALECTS {
        let live = live(dialect, Evidence::TypeIdWithoutCheck);
        for (surface, ir) in [
            ("table-level", &table_level),
            ("column-level", &column_level),
        ] {
            let error = lower(ir, dialect, &live).expect_err(
                "a chained typed reference omits its own CHECK and cannot be catalog-proved",
            );
            assert!(
                error.contains(MISSING_METADATA),
                "{surface} {dialect:?} chained TypeID target must stay rejected: {error}"
            );
        }
    }

    // A chained UUID reference on MySQL/SQLite carries UUID storage but no
    // CHECK. PostgreSQL has no equivalent shape: its native `uuid` type is the
    // contract, so a chained PostgreSQL UUID target is legitimately provable.
    let table_level = table_constraint_ir("table_level_chained_uuid", "uuid", None);
    let column_level = column_reference_ir("column_level_chained_uuid", "uuid", None);
    for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
        let live = live(dialect, Evidence::UuidWithoutCheck);
        for (surface, ir) in [
            ("table-level", &table_level),
            ("column-level", &column_level),
        ] {
            let error = lower(ir, dialect, &live)
                .expect_err("UUID storage without the engine's CHECK proves nothing");
            assert!(
                error.contains(MISSING_METADATA),
                "{surface} {dialect:?} chained UUID target must stay rejected: {error}"
            );
        }
    }
}
