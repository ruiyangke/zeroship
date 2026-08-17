//! **Populating `enum_values` from a named enum type must not put a CHECK in the DDL.**
//!
//! `fold_to_field_defs` now lifts a `ColType::Enum` column's members off the
//! `Op::CreateEnum` that declares them, so the runtime descriptor stops describing a
//! closed set as free text. The slot it fills has a SECOND reader:
//! `field_check_constraints` turns `enum_values` into `CHECK (<col> IN (...))`. Feeding
//! it input it never got from this path is exactly how a fix for one artifact becomes
//! a defect in the other, so the question is measured here rather than argued.
//!
//! WHAT THE DDL ALREADY DOES, by a different route. A named enum column's storage is
//! resolved from the `NamedTypeRegistry`, not from the descriptor:
//! `apply_named_type_column_metadata` gives PostgreSQL the NATIVE type
//! (`<schema>.<name>`, no CHECK at all), SQLite `TEXT` plus ONE inline
//! `CHECK ("col" IN (...))`, and MySQL an inlined `ENUM(...)`. A second CHECK arriving
//! from the descriptor would be a duplicate on SQLite and a redundant constraint on a
//! native enum on PostgreSQL.
//!
//! WHY IT CANNOT ARRIVE. `fold_to_field_defs` ends at `descriptor_to_sdk_schema`; the
//! DDL is built by `fold_ops` / the lower, which never see its `FieldDescriptor`s.
//! The one place its output IS load-bearing for DDL is the SQLite 12-step rebuild
//! (`engine`'s `live.sqlite_schemas`), and that case is measured below rather than
//! reasoned about. The measurement REFUTED the guess that prompted it: the rebuilt
//! CREATE is byte-identical with the lift disabled and enabled, because the rebuilt
//! column's membership comes from the desired snapshot's `inline_checks` - which
//! `fold_ops` put there from the `NamedTypeRegistry` - and not from the SDK `Value`
//! at all. So the descriptor's `enum_values` neither adds a CHECK nor restores one.

mod support;

use zero_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{fold_ops, fold_to_field_defs, PlanStep, RenameStep, SqlDialect};

const PROJECT: &str = "public";
const APP: &str = "app_enum";

/// `createEnum` + a table whose column is typed by it, plus a plain text control.
fn issues_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_issues",
        "owner_app": APP,
        "ops": [
            {
                "op": "createEnum",
                "name": "issue_status",
                "values": ["UNCONFIRMED", "CONFIRMED", "RESOLVED"],
            },
            {
                "op": "createTable",
                "name": "issues",
                "columns": [
                    {
                        "name": "status",
                        "type": { "enum": { "name": "issue_status" } },
                        "nullable": false,
                    },
                    { "name": "summary", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ],
    }))
    .expect("issues IR deserializes")
}

fn lowered_sql(dialect: SqlDialect, ir: &MigrationIr, live: &LiveSchema) -> String {
    IrAuthor::new(PROJECT, APP, dialect, &support::no_inject(PROJECT))
        .lower_steps(ir, live)
        .unwrap_or_else(|error| panic!("{dialect:?} lowers the enum table: {error}"))
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// PostgreSQL: the column IS the native type. Not one CHECK anywhere.
#[test]
fn a_native_enum_column_gets_no_membership_check() {
    let sql = lowered_sql(SqlDialect::Postgres, &issues_ir(), &LiveSchema::default());
    assert!(
        sql.contains(r#"CREATE TYPE "public"."issue_status""#)
            && sql.contains(r#""status" "public"."issue_status""#),
        "the enum is a native type and the column references it:\n{sql}"
    );
    assert_eq!(
        sql.matches("CHECK").count(),
        0,
        "a native enum column needs no membership CHECK, and must not gain one \
         from the descriptor's enum_values:\n{sql}"
    );
}

/// SQLite: exactly ONE inline membership CHECK, from the named-type metadata. The
/// descriptor's `enum_values` must not add a second.
#[test]
fn an_inlined_enum_column_gets_exactly_one_membership_check() {
    let sql = lowered_sql(SqlDialect::Sqlite, &issues_ir(), &LiveSchema::default());
    assert_eq!(
        sql.matches(r#"CHECK ("status" IN ("#).count(),
        1,
        "the inlined membership CHECK is emitted once, by the named-type metadata:\n{sql}"
    );
    assert_eq!(
        sql.matches("CHECK").count(),
        1,
        "and it is the only CHECK on the table:\n{sql}"
    );
}

/// The one place `fold_to_field_defs`' output IS load-bearing for DDL: the engine
/// seeds `live.sqlite_schemas` from it (`engine::refresh_historical_live`) and the
/// SQLite 12-step rebuild consumes that `Value`. So the descriptor now carrying a
/// membership has to be checked against real rebuild DDL, not argued about.
///
/// MEASURED, by rendering this rebuild with the lift disabled and with it enabled:
/// the CREATE is BYTE-IDENTICAL. A named enum column's membership reaches the rebuilt
/// table as the `ColumnSnapshot::inline_checks` entry `fold_ops` put there from the
/// `NamedTypeRegistry`, and the descriptor's `enum_values` adds nothing on top of it.
/// One CHECK before, one CHECK after.
///
/// A SECOND, PRE-EXISTING defect this measurement exposed and did NOT fix, kept here
/// because this test is where it was first seen: the inline CHECK's BODY still named
/// the PRE-rename column (`CHECK ("status" IN (...))` on a column now called `state`),
/// because `sqlite_rename_rebuild` renamed `ColumnSnapshot::name` and the generated
/// expressions but not `inline_checks`. It was byte-identical with and without the
/// membership lift, which is why it was recorded rather than blamed on it.
///
/// IT IS NOW FIXED, and the assertion below was REVERSED accordingly - it used to pin
/// the stale body as current behaviour. `rename_column_in_inline_checks` walks the
/// rendered fragment as quoted runs and rewrites only the identifier, which is why the
/// member literal `'status'` in the neighbouring file's fixture survives it. The
/// end-to-end proof, including what SQLite does when handed the stale body, is
/// `rename_column_inline_check_sqlite.rs`; this file keeps the assertion because the
/// membership lift must not reintroduce the stale spelling by another route.
#[test]
fn a_sqlite_rebuild_carries_the_membership_exactly_once() {
    let ops = issues_ir().ops;
    let effective = support::no_inject(PROJECT);
    let snapshot =
        fold_ops(&ops, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    // Seeded EXACTLY as `engine::refresh_historical_live` seeds it.
    live.sqlite_schemas = fold_to_field_defs(&ops, SqlDialect::Sqlite, PROJECT, &effective)
        .expect("the field-def replay folds");

    let rename = MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: "rename_status".to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::RenameColumn {
            table: "issues".to_string(),
            from: "status".to_string(),
            to: "state".to_string(),
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
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to one rebuild step: {steps:#?}");
    };
    let create = &rebuild.spec.new_table_create;

    assert_eq!(
        create.matches("CHECK").count(),
        1,
        "the rebuilt table carries the membership ONCE - the descriptor's enum_values \
         must not add a second CHECK beside the inline one:\n{create}"
    );
    assert!(
        create.contains("'UNCONFIRMED'")
            && create.contains("'CONFIRMED'")
            && create.contains("'RESOLVED'"),
        "and that one CHECK carries every member:\n{create}"
    );
    // And it names the POST-rename column. See the doc comment: this assertion was
    // reversed when the staleness it used to pin was fixed.
    assert!(
        create.contains(r#""state" TEXT NOT NULL CHECK ("state" IN ("#),
        "the inline CHECK body names the post-rename column:\n{create}"
    );
    assert!(
        !create.contains(r#"CHECK ("status" IN ("#),
        "and no CHECK body still names the renamed-away column:\n{create}"
    );
}
