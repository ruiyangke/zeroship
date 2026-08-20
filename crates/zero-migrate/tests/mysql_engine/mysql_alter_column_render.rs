//! MySQL alter-column ops: one is restated, one is refused, two are corrected. The
//! split is the point of this file.
//!
//! Every one of these renderers used to emit PostgreSQL syntax with double-quoted
//! identifiers on all three dialects, so nothing here executed on MySQL:
//!
//!     ALTER TABLE "app"."accounts" ALTER COLUMN "nickname" TYPE INT USING "nickname"::INT
//!     ALTER TABLE "app"."accounts" ALTER COLUMN "nickname" SET NOT NULL
//!
//! Note the TABLE is double-quoted too: PostgreSQL end to end, not a mix of the
//! two dialects.
//!
//! RESTATED - the type change (`setColumnType`). MySQL does it with `MODIFY COLUMN`,
//! which requires the COMPLETE column specification restated and silently DISCARDS
//! every facet left out. The op does not carry one: `Op::SetColumnType` builds its
//! snapshot through `add_column_snapshot(.., None, None, None, None, None, None,
//! None)`, called "a one-field descriptor" in its own comment. Rendering `MODIFY
//! COLUMN` from that would quietly drop the column's default, its NOT NULL, its
//! charset and its comment - measured against a live server in
//! `mysql_setcolumntype_restate.rs`, which loses all four.
//!
//! So the definition is not rendered from the op; it is READ, at apply, from
//! `SHOW CREATE TABLE`, and reproduced with only its type token replaced. That is
//! what `PlanStep::AlterColumnType` is for, and it is the shape
//! `apply/backend/mysql/primary_key_sql.rs` already used for `dropIdentityFrom`.
//!
//! REFUSED - the nullability change (`setColumnNotNull` / `dropColumnNotNull`). The
//! same restate would serve it, and this file does NOT claim otherwise: the ops are
//! still declared `unsupported` on MySQL because nobody has driven one end to end,
//! not because the engine cannot.
//!
//! CORRECTED - `SET DEFAULT` and `DROP DEFAULT`. MySQL spells these exactly the
//! way PostgreSQL does, so only the quoting was wrong. Refusing them would have
//! removed a capability MySQL has - the failure mode that has reversed several
//! fixes in this review - so they now render with backticks instead.
//!
//! Both lanes are covered because both reach the same renderers: the IR lane
//! through `Op::SetColumnType` / `SetColumnNotNull` / `DropColumnNotNull`
//! (authorable as `setColumnType`, `setColumnNotNull`, `dropColumnNotNull`), and
//! the declarative differ through its existing-table branch.
//!
//! PostgreSQL is asserted alongside every refusal. Without that half these tests
//! would still pass if the ops were refused on EVERY dialect, which would be a
//! regression rather than a fix.

use crate::support;

use std::collections::HashMap;

use zero_migrate::model::snapshot::SchemaSnapshot;
use zero_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
};
use zero_migrate::render::lower::IrAuthor;
use zero_migrate::{
    ColType, IrFlagsOverride, LiveSchema, MigrationIr, Op, PlanStep, SqlDialect, CURRENT_IR_VERSION,
};

const PROJECT: &str = "app";
const APP: &str = "app_test";

fn descriptor(ty: &str, required: bool) -> CollectionDescriptor {
    CollectionDescriptor {
        name: "accounts".to_string(),
        owner_app: APP.to_string(),
        fields: vec![FieldDescriptor {
            name: "nickname".to_string(),
            ty: ty.to_string(),
            required,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// The live snapshot for a table that already exists with `ty`, built through the
/// SAME desired-schema construction the differ uses, so the two sides differ in
/// exactly the facet under test rather than in how they were assembled.
fn live_with(
    ty: &str,
    required: bool,
    dialect: SqlDialect,
    effective: &zero_migrate_policy::EffectivePolicy,
) -> SchemaSnapshot {
    desired_snapshot_for_dialect(PROJECT, &[descriptor(ty, required)], dialect, effective)
        .expect("live-side desired snapshot")
        .snapshot
}

fn one_op_ir(op: Op) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "alter_nickname".to_string(),
        owner_app: APP.to_string(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn lower_steps_for(dialect: SqlDialect, op: Op) -> Result<Vec<zero_migrate::PlanStep>, String> {
    let author = IrAuthor::new(PROJECT, APP, dialect, &support::confined_charter());
    author
        .lower_steps(&one_op_ir(op), &LiveSchema::default())
        .map_err(|e| e.to_string())
}

fn lower_for(dialect: SqlDialect, op: Op) -> Result<(), String> {
    lower_steps_for(dialect, op).map(|_| ())
}

fn set_column_type_op() -> Op {
    Op::SetColumnType {
        table: "accounts".to_string(),
        column: "nickname".to_string(),
        to_type: ColType::Int,
        using: None,
        schema: None,
        existence_guard: None,
    }
}

fn set_column_not_null_op() -> Op {
    Op::SetColumnNotNull {
        table: "accounts".to_string(),
        column: "nickname".to_string(),
        schema: None,
        existence_guard: None,
    }
}

/// A MySQL retype lowers to a RESTATE step and emits no PostgreSQL syntax.
///
/// This test used to assert the refusal, and the refusal is gone. What it protected
/// is not: the property that matters here is that MySQL never receives
/// `ALTER COLUMN … TYPE`, and that is still asserted - more strongly, because it now
/// also has to hold for a lowering that SUCCEEDS. A refusal makes "emits no
/// PostgreSQL syntax" vacuously true.
///
/// The definition `MODIFY COLUMN` needs is read from `SHOW CREATE TABLE` at apply;
/// `tests/mysql_engine/mysql_setcolumntype_restate.rs` deploys one and reads every
/// facet back from a live server.
#[test]
fn the_ir_lane_restates_a_mysql_column_type_change_and_still_lowers_it_for_postgres() {
    let steps = lower_steps_for(SqlDialect::Mysql, set_column_type_op())
        .expect("MySQL lowers a retype to a restate step rather than refusing");
    assert_eq!(
        steps
            .iter()
            .filter(|step| matches!(step, PlanStep::AlterColumnType(_)))
            .count(),
        1,
        "expected exactly one restate step: {steps:?}"
    );
    assert!(
        !steps
            .iter()
            .any(|step| matches!(step, PlanStep::Ddl(m) if m.up.contains("ALTER COLUMN"))),
        "MySQL must never be handed PostgreSQL ALTER COLUMN syntax: {steps:?}"
    );

    // The control. A change that stopped PostgreSQL rendering its own retype would
    // satisfy the assertions above while breaking the dialect that has the statement.
    lower_for(SqlDialect::Postgres, set_column_type_op())
        .expect("PostgreSQL still lowers a column type change");
}

#[test]
fn the_ir_lane_refuses_a_mysql_nullability_change_and_still_lowers_it_for_postgres() {
    let refused = lower_for(SqlDialect::Mysql, set_column_not_null_op())
        .expect_err("MySQL must refuse rather than emit PostgreSQL SET NOT NULL");
    assert!(
        refused.contains("setColumnNotNull"),
        "the refusal names the authored op: {refused}"
    );

    lower_for(SqlDialect::Postgres, set_column_not_null_op())
        .expect("PostgreSQL still lowers a nullability change");
}

// The other half of the scope decision. `SET DEFAULT` / `DROP DEFAULT` are NOT
// refused, because MySQL spells them exactly the way PostgreSQL does and only the
// identifier quoting was wrong. MEASURED on MySQL 8.4.11:
//
//     ALTER TABLE zm87.t ALTER COLUMN `c` SET DEFAULT 'new'   accepted, COLUMN_DEFAULT = new
//     ALTER TABLE zm87.t ALTER COLUMN `c` DROP DEFAULT        accepted, COLUMN_DEFAULT NULL
//
// Refusing these would have removed a capability MySQL has, which is the failure
// mode this review has reversed fixes for before.
#[test]
fn a_mysql_default_change_is_rendered_with_backticks_rather_than_refused() {
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Mysql,
        &support::confined_charter(),
    );
    let steps = author
        .lower_steps(
            &one_op_ir(Op::SetColumnDefault {
                table: "accounts".to_string(),
                column: "nickname".to_string(),
                value: zero_migrate::IrDefault::Literal {
                    value: zero_migrate::IrScalar::Str("new".to_string()),
                },
                schema: None,
                existence_guard: None,
            }),
            &LiveSchema::default(),
        )
        .expect("MySQL renders a default change rather than refusing it");

    let up = steps
        .iter()
        .filter_map(|step| match step {
            zero_migrate::render::step::PlanStep::Ddl(m) => Some(m.up.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(";\n");

    assert!(
        up.contains("ALTER COLUMN `nickname` SET DEFAULT"),
        "the column is backtick-quoted, which is what MySQL accepts: {up}"
    );
    assert!(
        !up.contains('"'),
        "no double-quoted identifier survives on the MySQL leg: {up}"
    );
}

#[test]
fn the_declarative_differ_refuses_a_mysql_column_change_and_still_diffs_for_postgres() {
    let effective = support::confined_charter();

    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[descriptor("integer", true)],
        SqlDialect::Mysql,
        &effective,
    )
    .expect("desired snapshot");
    let live = live_with("string", true, SqlDialect::Mysql, &effective);

    let err = DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Mysql)
        .diff(&desired, &live, &HashMap::new(), &[], &effective)
        .expect_err("the differ must refuse a MySQL column change rather than plan invalid DDL");
    let text = err.to_string();
    assert!(
        text.to_lowercase().contains("mysql"),
        "the refusal names the dialect: {text}"
    );
    assert!(
        text.contains("accounts") && text.contains("nickname"),
        "and names the table and column so the operator can act on it: {text}"
    );

    // The same control on the declarative side.
    let pg_desired = desired_snapshot_for_dialect(
        PROJECT,
        &[descriptor("integer", true)],
        SqlDialect::Postgres,
        &effective,
    )
    .expect("desired snapshot");
    let pg_live = live_with("string", true, SqlDialect::Postgres, &effective);
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Postgres)
        .diff(&pg_desired, &pg_live, &HashMap::new(), &[], &effective)
        .expect("PostgreSQL still diffs a column type change");
}
