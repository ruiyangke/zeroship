//! A column rename must follow the generated expressions that READ that column,
//! in the descriptor lane that produces `schema.runtime.json`.
//!
//! `fold_to_field_defs` keeps a generated column's expression as STRUCTURED IR
//! (`GeneratedCol.expr` is a closed `Expr`, never rendered SQL), which is what makes
//! this fixable at all: the rename walks the AST and matches only column references,
//! so a string literal spelling the old name is left alone. That is the same
//! discrimination the drop-column cascade already relies on, and the reason text
//! substitution is refused everywhere else in this crate.
//!
//! Two carriers are pinned here, both reached from the same `Op::RenameColumn` arm:
//! the generated expression itself, and the recovered CHECK/foreign-key facets that
//! are lifted onto columns BY NAME after the fold has walked the op stream. A lift
//! that still searches for the old name silently drops the facet rather than
//! failing, so `min`/`max` and `onDelete`/`onUpdate` simply vanish.
//!
//! The offline `SchemaSnapshot` lane is a different story and is NOT covered here:
//! it renders the expression to a `String` at construction, nothing compares it, and
//! nothing executes it. See `fold_rename_column_stale_index_body_pg.rs` for the
//! equivalent reasoning about index bodies.

mod support;

use zero_migrate::{
    fold_to_field_defs, BinaryOp, ColType, Expr, GeneratedCol, IrColumn, IrConstraint,
    IrConstraintKind, IrScalar, Op, SqlDialect,
};

const SCHEMA: &str = "app";

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

/// `qty * unit_cents`, the expression whose `qty` reference the rename has to follow.
fn total_from_qty() -> GeneratedCol {
    GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Mul,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::col("unit_cents")),
        },
        stored: true,
    }
}

fn create_line_items() -> Op {
    let mut total = col("total_cents", ColType::Int);
    total.generated = Some(total_from_qty());
    Op::CreateTable {
        name: "line_items".to_string(),
        columns: vec![
            col("qty", ColType::Int),
            col("unit_cents", ColType::Int),
            total,
        ],
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn rename_qty_to_quantity() -> Op {
    Op::RenameColumn {
        table: "line_items".to_string(),
        from: "qty".to_string(),
        to: "quantity".to_string(),
        ty: ColType::Int,
        schema: None,
        existence_guard: None,
    }
}

/// Every column reference inside `value`, by name, in document order.
fn col_refs(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("node").and_then(serde_json::Value::as_str) == Some("colRef") {
                if let Some(name) = map.get("name").and_then(serde_json::Value::as_str) {
                    found.push(name.to_string());
                }
            }
            for nested in map.values() {
                col_refs(nested, found);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                col_refs(nested, found);
            }
        }
        _ => {}
    }
}

#[test]
fn a_rename_follows_the_generated_expressions_that_read_the_column() {
    let effective = support::confined_charter();
    let ops = vec![create_line_items(), rename_qty_to_quantity()];
    let fields = fold_to_field_defs(&ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("the op stream folds");

    let table = &fields["line_items"];
    assert!(
        table.get("quantity").is_some(),
        "the renamed column is present under its new name: {table}"
    );
    assert!(
        table.get("qty").is_none(),
        "the old column name is gone: {table}"
    );

    let generated = table["total_cents"]
        .get("generated")
        .unwrap_or_else(|| panic!("the generated column keeps its expression: {table}"));

    let mut names = Vec::new();
    col_refs(generated, &mut names);
    assert!(
        !names.is_empty(),
        "the expression carries column references to check: {generated}"
    );
    assert!(
        !names.iter().any(|n| n == "qty"),
        "no column reference may still name the renamed-away column, or the shipped \
         runtime descriptor describes a column the database does not have: {generated}"
    );
    assert!(
        names.iter().any(|n| n == "quantity"),
        "the reference follows the rename to the new name: {generated}"
    );
    assert!(
        names.iter().any(|n| n == "unit_cents"),
        "an untouched reference is left alone: {generated}"
    );
}

/// `qty >= 0 AND qty <= 100`, the bound whose recovered facet is lifted by name.
fn qty_between_0_and_100() -> Expr {
    Expr::BinOp {
        op: BinaryOp::And,
        lhs: Box::new(Expr::BinOp {
            op: BinaryOp::Ge,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::Literal {
                value: IrScalar::Int(0),
            }),
        }),
        rhs: Box::new(Expr::BinOp {
            op: BinaryOp::Le,
            lhs: Box::new(Expr::col("qty")),
            rhs: Box::new(Expr::Literal {
                value: IrScalar::Int(100),
            }),
        }),
    }
}

fn create_bounded() -> Op {
    Op::CreateTable {
        name: "line_items".to_string(),
        columns: vec![col("qty", ColType::Int)],
        primary_key: None,
        constraints: vec![IrConstraint {
            name: Some("line_items_qty_range".to_string()),
            kind: IrConstraintKind::Check {
                expr: qty_between_0_and_100(),
                not_valid: None,
            },
        }],
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

// A recovered CHECK facet is lifted onto its column by name once the whole op stream
// has been walked, so a rename that does not carry the pending facet's name forward
// leaves the lift searching for a column that is no longer there. The lift looks the
// column up rather than failing, so the bound does not error - it silently vanishes.
#[test]
fn a_rename_carries_a_recovered_check_bound_onto_the_new_column_name() {
    let effective = support::confined_charter();

    let before = fold_to_field_defs(
        &[create_bounded()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the unrenamed stream folds");
    let bounded = &before["line_items"]["qty"];
    assert_eq!(
        bounded.get("min").and_then(serde_json::Value::as_f64),
        Some(0.0),
        "the control: the bound is recovered without a rename: {bounded}"
    );
    assert_eq!(
        bounded.get("max").and_then(serde_json::Value::as_f64),
        Some(100.0),
        "the control: the bound is recovered without a rename: {bounded}"
    );

    let after = fold_to_field_defs(
        &[create_bounded(), rename_qty_to_quantity()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the renamed stream folds");
    let renamed = &after["line_items"]["quantity"];
    assert_eq!(
        renamed.get("min").and_then(serde_json::Value::as_f64),
        Some(0.0),
        "the bound survives the rename onto the new column name: {renamed}"
    );
    assert_eq!(
        renamed.get("max").and_then(serde_json::Value::as_f64),
        Some(100.0),
        "the bound survives the rename onto the new column name: {renamed}"
    );
}
