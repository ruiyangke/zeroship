//! Catalog-proved UUID formats for references into an unmanaged live target.
//!
//! A format-bearing local column may reference a target that has no authored
//! contract when the live catalog carries the target's own format evidence:
//! PostgreSQL's native `uuid` type, or the engine's exact UUID spelling CHECK on
//! MySQL/SQLite. A target that omits its own CHECK and inherits safety through
//! its foreign key carries no such evidence and stays rejected.
//!
//! Both reference surfaces are exercised: the column-level `IrColumn.references`
//! facet and the table-level single-column `constraints[].kind = "fk"` facet.
//! They run through separate validation loops, so each needs its own coverage.

use crate::support;

use serde_json::{json, Value};
use zeroship_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zeroship_migrate::{
    ColumnSnapshot, ConstraintSnapshot, IrAuthor, LiveSchema, SchemaSnapshot, TableSnapshot,
    TextStorageSnapshot,
};

const PROJECT_SCHEMA: &str = "app";
const OWNER: &str = "app_catalog_format_proof";
const PARENT: &str = "unmanaged_parents";
const MISSING_METADATA: &str = "no authored value-format metadata";

fn no_inject_policy() -> zeroship_migrate::EffectivePolicy {
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
    /// Generic unmanaged text with no format contract of any kind.
    PlainText,
}

fn ascii_bin() -> TextStorageSnapshot {
    TextStorageSnapshot {
        character_set: "ascii".to_string(),
        collation: "ascii_bin".to_string(),
    }
}

fn target_column(dialect: &zeroship_migrate::DialectId, evidence: Evidence) -> ColumnSnapshot {
    let mut column = ColumnSnapshot {
        name: "id".to_string(),
        nullable: false,
        ..Default::default()
    };
    match evidence {
        Evidence::Uuid | Evidence::UuidWithoutCheck => {
            if dialect == &zeroship_migrate_postgres::DIALECT {
                column.data_type = "uuid".to_string();
            } else if dialect == &zeroship_migrate_mysql::DIALECT {
                column.data_type = "varchar(36)".to_string();
                column.text_storage = Some(ascii_bin());
                column.catalog_uuid_format_check = matches!(evidence, Evidence::Uuid);
            } else if dialect == &zeroship_migrate_sqlite::DIALECT {
                column.data_type = "text".to_string();
                column.catalog_uuid_format_check = matches!(evidence, Evidence::Uuid);
            } else {
                panic!("unregistered test dialect {dialect}");
            }
        }
        Evidence::PlainText => {
            if dialect == &zeroship_migrate_mysql::DIALECT {
                column.data_type = "varchar(191)".to_string();
            } else if dialect == &zeroship_migrate_postgres::DIALECT
                || dialect == &zeroship_migrate_sqlite::DIALECT
            {
                column.data_type = "text".to_string();
            } else {
                panic!("unregistered test dialect {dialect}");
            }
        }
    }
    column
}

fn live(dialect: &zeroship_migrate::DialectId, evidence: Evidence) -> LiveSchema {
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
            attributes: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        },
    );
    LiveSchema::from_catalog_snapshot(snapshot, "external_owner")
}

fn local_column(ty: &str, references: Option<Value>) -> Value {
    let mut column = json!({
        "name": "parent_id",
        "type": ty,
        "nullable": true,
    });
    let object = column.as_object_mut().expect("column fixture is an object");
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
fn column_reference_ir(name: &str, ty: &str) -> MigrationIr {
    ir(
        name,
        vec![local_column(
            ty,
            Some(json!({ "table": PARENT, "column": "id" })),
        )],
        Vec::new(),
    )
}

/// The table-level single-column foreign-key surface.
fn table_constraint_ir(name: &str, ty: &str) -> MigrationIr {
    ir(
        name,
        vec![local_column(ty, None)],
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

fn lower(
    ir: &MigrationIr,
    dialect: &zeroship_migrate::DialectId,
    live: &LiveSchema,
) -> Result<(), String> {
    IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT_SCHEMA,
        OWNER,
        dialect,
        &no_inject_policy(),
    )
    .lower(ir, live)
    .map(|_| ())
    .map_err(|error| error.to_string())
}

const DIALECTS: [&zeroship_migrate::DialectId; 3] = [
    &zeroship_migrate_postgres::DIALECT,
    &zeroship_migrate_mysql::DIALECT,
    &zeroship_migrate_sqlite::DIALECT,
];

#[test]
fn catalog_uuid_evidence_proves_a_table_level_single_column_foreign_key() {
    let ir = table_constraint_ir("table_level_uuid_fk", "uuid");
    for dialect in DIALECTS {
        lower(&ir, dialect, &live(dialect, Evidence::Uuid)).unwrap_or_else(|error| {
            panic!("live UUID evidence must prove the {dialect:?} table-level FK: {error}")
        });
    }
}

#[test]
fn catalog_uuid_evidence_proves_a_column_level_reference() {
    let ir = column_reference_ir("column_level_uuid_reference", "uuid");
    for dialect in DIALECTS {
        lower(&ir, dialect, &live(dialect, Evidence::Uuid)).unwrap_or_else(|error| {
            panic!("live UUID evidence must prove the {dialect:?} column reference: {error}")
        });
    }
}

#[test]
fn a_live_text_target_without_uuid_evidence_stays_rejected() {
    let table_level = table_constraint_ir("table_level_uuid_no_evidence", "uuid");
    let column_level = column_reference_ir("column_level_uuid_no_evidence", "uuid");
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
fn a_chained_uuid_reference_target_without_its_own_check_stays_rejected() {
    // A chained UUID reference on MySQL/SQLite carries UUID storage but no
    // CHECK. PostgreSQL has no equivalent shape: its native `uuid` type is the
    // contract, so a chained PostgreSQL UUID target is legitimately provable.
    let table_level = table_constraint_ir("table_level_chained_uuid", "uuid");
    let column_level = column_reference_ir("column_level_chained_uuid", "uuid");
    for dialect in [&zeroship_migrate_mysql::DIALECT, &zeroship_migrate_sqlite::DIALECT] {
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
