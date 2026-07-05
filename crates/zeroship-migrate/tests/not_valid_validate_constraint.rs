//! Regression coverage for PostgreSQL online constraint adoption (the additive
//! DSL-redesign slice): the `not_valid` facet on FK/CHECK constraints and the new
//! `Op::ValidateConstraint`.
//!
//! Gates:
//!   1. RENDER (PG) — `ADD CONSTRAINT … FOREIGN KEY … NOT VALID`,
//!      `ADD CONSTRAINT … CHECK (…) NOT VALID`, `ALTER TABLE … VALIDATE CONSTRAINT`.
//!   2. VALIDATE (fail-closed) — `notValid` on FK/CHECK is REFUSED on SQLite/MySQL;
//!      `Op::ValidateConstraint` is REFUSED off Postgres; both are accepted on PG.
//!   3. RECORDER — the `constraint_not_valid` corpus fixture (op_round_trip.rs) is
//!      the byte-stable REAL-recorder gate for `addForeignKey/addCheck { notValid }`
//!      + `.validateConstraint(name)`; this file asserts the recorded golden shape.

use std::path::PathBuf;

use zeroship_migrate::model::expr::{BinaryOp, Expr};
use zeroship_migrate::model::ir::{
    IrConstraint, IrConstraintKind, IrScalar, MigrationIr, Op,
};
use zeroship_migrate::model::support::Dialect;
use zeroship_migrate::model::validate::validate_ir_scoped;
use zeroship_migrate::{
    IrAuthor, IrFlagsOverride, LiveSchema, PolicyProfile, SchemaScope, SqlDialect,
    CURRENT_IR_VERSION,
};

fn ir(op: Op) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "not_valid".into(),
        owner_app: "app_nv".into(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn pg_sql(op: Op) -> Vec<String> {
    IrAuthor::new("app", "app_nv", SqlDialect::Postgres)
        .lower(&ir(op), &LiveSchema::default())
        .expect("lower on Postgres")
        .into_iter()
        .map(|m| m.up)
        .collect()
}

fn validates(op: Op, dialect: Dialect) -> bool {
    validate_ir_scoped(
        &ir(op),
        dialect,
        &[],
        Some(&SchemaScope::Unconfined),
        &PolicyProfile::platform(),
    )
    .is_ok()
}

fn fk_not_valid(not_valid: Option<bool>) -> Op {
    Op::AddConstraint {
        table: "line_items".into(),
        constraint: IrConstraint {
            name: Some("line_items_order_fkey".into()),
            kind: IrConstraintKind::Fk {
                columns: vec!["order_id".into()],
                references_table: "orders".into(),
                references_columns: vec!["id".into()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,
                not_valid,
            },
        },
        schema: None,
        existence_guard: None,
    }
}

fn check_not_valid(not_valid: Option<bool>) -> Op {
    Op::AddConstraint {
        table: "line_items".into(),
        constraint: IrConstraint {
            name: Some("line_items_qty_positive".into()),
            kind: IrConstraintKind::Check {
                expr: Expr::BinOp {
                    op: BinaryOp::Gt,
                    lhs: Box::new(Expr::ColRef { name: "qty".into(), table: None }),
                    rhs: Box::new(Expr::Literal { value: IrScalar::Int(0) }),
                },
                not_valid,
            },
        },
        schema: None,
        existence_guard: None,
    }
}

fn validate_constraint() -> Op {
    Op::ValidateConstraint {
        table: "line_items".into(),
        name: "line_items_order_fkey".into(),
        schema: None,
        existence_guard: None,
    }
}

// ── 1. RENDER (Postgres) ────────────────────────────────────────────────────

#[test]
fn pg_add_foreign_key_not_valid_renders_not_valid_tail() {
    let sql = pg_sql(fk_not_valid(Some(true)));
    let up = sql.join(";\n");
    assert!(up.contains("ADD CONSTRAINT"), "{up}");
    assert!(up.contains("FOREIGN KEY"), "{up}");
    assert!(up.trim_end().ends_with("NOT VALID"), "FK body must end NOT VALID: {up}");
}

#[test]
fn pg_add_check_not_valid_renders_not_valid_tail() {
    let sql = pg_sql(check_not_valid(Some(true)));
    let up = sql.join(";\n");
    assert!(up.contains("CHECK ("), "{up}");
    assert!(up.trim_end().ends_with("NOT VALID"), "CHECK body must end NOT VALID: {up}");
}

#[test]
fn pg_plain_add_constraint_omits_not_valid() {
    // Absent `not_valid` must be checksum-neutral: no NOT VALID in the rendered DDL.
    let fk = pg_sql(fk_not_valid(None)).join(";\n");
    assert!(!fk.contains("NOT VALID"), "plain FK must not render NOT VALID: {fk}");
    let ck = pg_sql(check_not_valid(None)).join(";\n");
    assert!(!ck.contains("NOT VALID"), "plain CHECK must not render NOT VALID: {ck}");
}

#[test]
fn pg_validate_constraint_renders_validate_constraint() {
    let up = pg_sql(validate_constraint()).join(";\n");
    assert!(up.contains("VALIDATE CONSTRAINT"), "{up}");
    assert!(up.contains("ALTER TABLE"), "{up}");
    assert!(up.contains("line_items_order_fkey"), "{up}");
}

// ── 2. VALIDATE (fail-closed off Postgres) ──────────────────────────────────

#[test]
fn not_valid_fk_is_postgres_only() {
    assert!(validates(fk_not_valid(Some(true)), Dialect::Postgres));
    assert!(!validates(fk_not_valid(Some(true)), Dialect::Sqlite));
    assert!(!validates(fk_not_valid(Some(true)), Dialect::Mysql));
    // A plain FK (no notValid) is still portable to PG + MySQL.
    assert!(validates(fk_not_valid(None), Dialect::Postgres));
    assert!(validates(fk_not_valid(None), Dialect::Mysql));
}

#[test]
fn not_valid_check_is_postgres_only() {
    assert!(validates(check_not_valid(Some(true)), Dialect::Postgres));
    assert!(!validates(check_not_valid(Some(true)), Dialect::Sqlite));
    assert!(!validates(check_not_valid(Some(true)), Dialect::Mysql));
}

#[test]
fn validate_constraint_op_is_postgres_only() {
    assert!(validates(validate_constraint(), Dialect::Postgres));
    assert!(!validates(validate_constraint(), Dialect::Sqlite));
    assert!(!validates(validate_constraint(), Dialect::Mysql));
}

#[test]
fn not_valid_on_create_time_constraint_is_refused_everywhere() {
    // NOT VALID is meaningless at create-time; refused fail-closed on every dialect.
    let create = |not_valid: Option<bool>| {
        Op::CreateTable {
            name: "line_items".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some("line_items_qty_positive".into()),
                kind: IrConstraintKind::Check {
                    expr: Expr::BinOp {
                        op: BinaryOp::Gt,
                        lhs: Box::new(Expr::ColRef { name: "qty".into(), table: None }),
                        rhs: Box::new(Expr::Literal { value: IrScalar::Int(0) }),
                    },
                    not_valid,
                },
            }],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    };
    assert!(!validates(create(Some(true)), Dialect::Postgres));
}

// ── 3. RECORDER (the surface builders) ──────────────────────────────────────

#[test]
fn recorder_golden_carries_not_valid_and_validate_constraint() {
    // The op_round_trip corpus records `constraint_not_valid.mig.js` via the REAL
    // V8 recorder; this asserts the committed golden shape the surface produces.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/op_fixtures/constraint_not_valid.golden.json");
    let golden: MigrationIr = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("read constraint_not_valid.golden.json"),
    )
    .expect("parse golden");

    // FK + CHECK carry notValid; two validateConstraint ops follow.
    let fk_not_valid = golden.ops.iter().any(|op| {
        matches!(op, Op::AddConstraint { constraint, .. }
            if matches!(&constraint.kind, IrConstraintKind::Fk { not_valid: Some(true), .. }))
    });
    let check_not_valid = golden.ops.iter().any(|op| {
        matches!(op, Op::AddConstraint { constraint, .. }
            if matches!(&constraint.kind, IrConstraintKind::Check { not_valid: Some(true), .. }))
    });
    let validate_count = golden
        .ops
        .iter()
        .filter(|op| matches!(op, Op::ValidateConstraint { .. }))
        .count();

    assert!(fk_not_valid, "golden must record an FK with notValid");
    assert!(check_not_valid, "golden must record a CHECK with notValid");
    assert_eq!(validate_count, 2, "golden must record two validateConstraint ops");
}
