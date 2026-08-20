//! **Reporting a domain column's base type must not change one byte of DDL.**
//!
//! The `FieldDef` projection resolves a `ColType::Domain` column's BASE TYPE off the
//! `Op::CreateDomain` that declares it, so the runtime descriptor stops describing an
//! integer column as text. That slot has other readers: `field_data_type` picks the
//! rendered storage from `ty`, and `field_check_constraints` turns `min`/`max` into a
//! `CHECK`. Feeding them input they never got from this path is how a fix for one
//! artifact becomes a defect in the other, so it is measured here rather than argued.
//!
//! WHAT THE DDL ALREADY DOES, by a different route. The storage comes from the
//! `NamedTypeRegistry` via `apply_named_type_column_metadata` /
//! `apply_fold_named_type_column_metadata`, never from a `FieldDescriptor`.
//!
//! THE ONE PLACE THIS REPLAY IS LOAD-BEARING FOR DDL is the SQLite 12-step rebuild -
//! `engine` seeds `live.sqlite_schemas` from the `FieldDef` projection and the rebuild
//! renders `CREATE TABLE` from that `Value`. Measured with the lift disabled and
//! enabled: the rebuilt CREATE is BYTE-IDENTICAL, because the rebuilt column's storage
//! and its inline CHECK both come from the `ColumnSnapshot` `fold_ops` produced.
//!
//! AND THE REPLAY AGREEMENT the three folds owe each other: `fold_ops` said the column
//! stores an integer, `authoring_tables_from_ops` said `t.domain("positive_number")`,
//! and `fold_to_field_defs` said `"string"`. Two of the three were right. The third is
//! now pinned against them here.

use crate::support;

use zero_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zero_migrate::render::fold::single_fold;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{fold_ops, PlanStep, RenameStep, SqlDialect};

const PROJECT: &str = "public";
const APP: &str = "app_domain";

/// `createDomain positive_number AS int CHECK (VALUE > 0)`, a column of it, and a
/// plain `int` control that must keep its own storage.
fn amounts_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_amounts",
        "owner_app": APP,
        "ops": [
            {
                "op": "createDomain",
                "name": "positive_number",
                "as": "int",
                "check": {
                    "node": "binOp",
                    "op": "gt",
                    "lhs": { "node": "colRef", "name": "VALUE" },
                    "rhs": { "node": "literal", "value": 0 },
                },
            },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    {
                        "name": "amount",
                        "type": { "domain": { "name": "positive_number" } },
                        "nullable": false,
                    },
                    { "name": "weight", "type": "int", "nullable": false },
                    { "name": "note", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ],
    }))
    .expect("amounts IR deserializes")
}

fn lowered_sql(dialect: SqlDialect) -> String {
    IrAuthor::new(PROJECT, APP, dialect, &support::no_inject(PROJECT))
        .lower_steps(&amounts_ir(), &LiveSchema::default())
        .unwrap_or_else(|error| panic!("{dialect:?} lowers the domain table: {error}"))
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// PostgreSQL: the column IS the native domain type and the constraint lives on the
/// DOMAIN, so the table carries no CHECK at all. The descriptor's new `"int"` must not
/// put one there.
#[test]
fn a_native_domain_column_keeps_its_type_reference_and_gains_no_check() {
    let sql = lowered_sql(SqlDialect::Postgres);
    assert!(
        sql.contains(r#"CREATE DOMAIN "public"."positive_number" AS integer CHECK ((VALUE > 0))"#),
        "the domain is a native type over `integer`:\n{sql}"
    );
    assert!(
        sql.contains(r#""amount" "public"."positive_number" NOT NULL"#),
        "and the column references it, rather than being rendered as its base:\n{sql}"
    );
    assert_eq!(
        sql.matches(r#"CREATE TABLE"#).count(),
        1,
        "one table is created:\n{sql}"
    );
    let create = sql
        .lines()
        .find(|line| line.contains("CREATE TABLE"))
        .expect("the CREATE TABLE is on its own line");
    assert_eq!(
        create.matches("CHECK").count(),
        0,
        "the constraint is the DOMAIN's, so the table gets no CHECK - and must not \
         gain one from the descriptor:\n{create}"
    );
}

/// SQLite and MySQL INLINE the domain: the base type becomes the column's storage and
/// the domain's predicate becomes exactly ONE inline CHECK, with `VALUE` rewritten to
/// the use-site column. That is the shape the runtime descriptor was contradicting.
#[test]
fn an_inlined_domain_column_stores_the_base_type_with_exactly_one_check() {
    let sqlite = lowered_sql(SqlDialect::Sqlite);
    assert!(
        sqlite.contains(r#""amount" INTEGER NOT NULL CHECK (("amount" > 0))"#),
        "SQLite stores the base type and inlines the predicate once:\n{sqlite}"
    );
    assert_eq!(
        sqlite.matches("CHECK").count(),
        1,
        "and it is the only CHECK on the table:\n{sqlite}"
    );
    assert!(
        sqlite.contains(r#""weight" INTEGER NOT NULL"#)
            && sqlite.contains(r#""note" TEXT NOT NULL"#),
        "the plain controls are untouched:\n{sqlite}"
    );

    let mysql = lowered_sql(SqlDialect::Mysql);
    assert!(
        mysql.contains("`amount` INT NOT NULL CHECK ((`amount` > 0))"),
        "MySQL does the same with its own spelling:\n{mysql}"
    );
    assert_eq!(mysql.matches("CHECK").count(), 1, "and once only:\n{mysql}");
}

/// THE REPLAY AGREEMENT. `fold_ops` and the `FieldDef` projection describe the SAME op stream
/// and must not describe the same column differently. Before this change `fold_ops`
/// said the SQLite column stores `INTEGER` while `fold_to_field_defs` said `"string"`.
#[test]
fn the_snapshot_fold_and_the_field_def_fold_agree_about_the_storage() {
    let ops = amounts_ir().ops;
    let effective = support::no_inject(PROJECT);

    for (dialect, expected_data_type) in [
        // The inlining dialects render the BASE type into the column.
        (SqlDialect::Sqlite, "integer"),
        (SqlDialect::Mysql, "integer"),
        // PostgreSQL keeps the NAMED type; the descriptor's job is to say what that
        // name is a domain OVER.
        (SqlDialect::Postgres, "public.positive_number"),
    ] {
        let snapshot = fold_ops(&ops, dialect, PROJECT, &effective).expect("the history folds");
        let column = snapshot.tables["amounts"]
            .columns
            .iter()
            .find(|c| c.name == "amount")
            .expect("the domain column is in the snapshot");
        assert_eq!(
            column.data_type, expected_data_type,
            "{dialect:?}: the snapshot fold's storage for the domain column"
        );

        let defs = single_fold::fold(&ops, dialect, PROJECT, &effective)
            .map(|folded| folded.project_field_defs())
            .expect("the field-def replay folds");
        assert_eq!(
            defs["amounts"]["amount"]["type"], "int",
            "{dialect:?}: and the field-def replay agrees it is an integer, instead of \
             calling it a string: {}",
            defs["amounts"]
        );
        assert_eq!(
            defs["amounts"]["weight"]["type"], "int",
            "{dialect:?}: the plain control is unchanged: {}",
            defs["amounts"]
        );
        assert_eq!(
            defs["amounts"]["note"]["type"], "string",
            "{dialect:?}: and `string` is still reachable: {}",
            defs["amounts"]
        );
    }
}

/// The one place the `FieldDef` map IS load-bearing for DDL: the engine seeds
/// `live.sqlite_schemas` from it and the SQLite 12-step rebuild renders `CREATE TABLE`
/// from that `Value`. So the descriptor now carrying `"int"` has to be checked against
/// real rebuild DDL, not argued about.
///
/// MEASURED with the lift disabled and enabled: BYTE-IDENTICAL. The rebuilt column's
/// `INTEGER` storage and its single inline CHECK both come from the `ColumnSnapshot`
/// `fold_ops` produced from the `NamedTypeRegistry`, and the descriptor adds nothing on
/// top of either.
#[test]
fn a_sqlite_rebuild_keeps_the_domain_storage_and_one_check() {
    let ops = amounts_ir().ops;
    let effective = support::no_inject(PROJECT);
    let snapshot =
        fold_ops(&ops, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    // Seeded EXACTLY as `engine::refresh_historical_live` seeds it.
    live.sqlite_schemas = single_fold::fold(&ops, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the field-def replay folds");

    let rename = MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: "rename_note".to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::RenameColumn {
            table: "amounts".to_string(),
            from: "note".to_string(),
            to: "memo".to_string(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };

    let steps = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective)
        .lower_steps(&rename, &live)
        .expect("the rename lowers to a rebuild");
    let [PlanStep::OnlineRename(RenameStep::TableRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to one rebuild step: {steps:#?}");
    };
    let create = &rebuild.spec.new_table_create;

    assert_eq!(
        create,
        r#"CREATE TABLE "amounts__zero_migrate_rebuild" ("amount" INTEGER NOT NULL CHECK (("amount" > 0)), "weight" INTEGER NOT NULL, "memo" TEXT NOT NULL)"#,
        "the rebuilt table is byte-identical to what it was before the descriptor \
         learned the base type"
    );
}
