//! A column rename must follow the generated expressions that READ that column,
//! in the descriptor lane that produces `schema.runtime.json`.
//!
//! The `FieldDef` projection keeps a generated column's expression as STRUCTURED IR
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
//! The offline `SchemaSnapshot` lane follows the rename too, and the last test here
//! pins that. It did not always: the snapshot rendered the expression to a `String`
//! at construction and threw the AST away, so the rename had nothing structural to
//! walk. It now keeps the closed `Expr` beside the rendering
//! (`GeneratedColumnSnapshot::source`) for that reason alone — which is the treatment
//! `apply::drift::comparable_generated_column` prescribes, and the one an INDEX body
//! still does not get (see `fold_rename_column_stale_index_body_pg.rs`: a predicate
//! and an expression key remain stale because the snapshot keeps no AST for them).

mod support;

use zero_migrate::render::fold::single_fold;
use zero_migrate::{
    diff_snapshots, fold_ops, BinaryOp, ColType, Expr, GeneratedCol, IrColumn, IrConstraint,
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
        collation: None,
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
    let fields = single_fold::fold(&ops, SqlDialect::Postgres, SCHEMA, &effective)
        .map(|folded| folded.project_field_defs())
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

/// `line_items.qty * unit_cents` — the same expression, with the reference to the
/// renamed-away column QUALIFIED by its enclosing table.
fn total_from_qualified_qty() -> GeneratedCol {
    GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Mul,
            lhs: Box::new(Expr::ColRef {
                table: Some("line_items".to_string()),
                name: "qty".to_string(),
            }),
            rhs: Box::new(Expr::col("unit_cents")),
        },
        stored: true,
    }
}

fn create_qualified_line_items() -> Op {
    let mut total = col("total_cents", ColType::Int);
    total.generated = Some(total_from_qualified_qty());
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

/// Every QUALIFIER inside `value`, by name, in document order.
fn col_ref_tables(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("node").and_then(serde_json::Value::as_str) == Some("colRef") {
                if let Some(table) = map.get("table").and_then(serde_json::Value::as_str) {
                    found.push(table.to_string());
                }
            }
            for nested in map.values() {
                col_ref_tables(nested, found);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                col_ref_tables(nested, found);
            }
        }
        _ => {}
    }
}

// A generated expression may QUALIFY its column references with the enclosing table,
// and a TABLE rename moves that qualifier exactly as it moves the collection key. The
// `env.db.ts` replay in `render::gen_types` has rewritten it all along; this descriptor
// fold did not, so the two artifacts shipped SIDE BY SIDE out of one `renderArtifacts`
// call described the same column under different table names — the runtime descriptor
// still naming a collection that no longer exists.
//
// Asserted through `render_artifacts`, which produces both, so the test fails if
// EITHER lane regresses rather than only the one being repaired.
#[test]
fn a_table_rename_carries_a_qualified_generated_reference_in_both_artifacts() {
    let effective = support::confined_charter();
    let ops = vec![
        create_qualified_line_items(),
        Op::RenameTable {
            table: "line_items".to_string(),
            to: "order_lines".to_string(),
            schema: None,
            existence_guard: None,
        },
    ];

    let artifacts = zero_migrate::render_artifacts(&ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("the op stream renders both artifacts");

    let runtime: serde_json::Value =
        serde_json::from_str(&artifacts.runtime_json).expect("the runtime descriptor is JSON");
    let generated = runtime["collections"]["order_lines"]["fields"]["total_cents"]
        .get("generated")
        .unwrap_or_else(|| panic!("the generated column keeps its expression: {runtime}"));
    let mut qualifiers = Vec::new();
    col_ref_tables(generated, &mut qualifiers);
    assert_eq!(
        qualifiers,
        vec!["order_lines".to_string()],
        "the runtime descriptor's qualifier follows the table rename: {generated}"
    );

    // The authoring artifact, from the OTHER replay over the same ops.
    assert!(
        artifacts.env_db_ts.contains("\"table\":\"order_lines\""),
        "the authoring types name the renamed table too: {}",
        artifacts.env_db_ts
    );
    assert!(
        !artifacts.env_db_ts.contains("\"table\":\"line_items\""),
        "no authoring reference may still name the pre-rename table: {}",
        artifacts.env_db_ts
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

    let before = single_fold::fold(
        &[create_bounded()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .map(|folded| folded.project_field_defs())
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

    let after = single_fold::fold(
        &[create_bounded(), rename_qty_to_quantity()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .map(|folded| folded.project_field_defs())
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

// The OTHER lane, which used to be excluded here and no longer is.
//
// THIS TEST WAS REVERSED, DELIBERATELY. It previously asserted that the snapshot lane
// KEEPS a stale generated body, on the stated grounds that "no reader exists". That
// grounds is a testable claim and it was measured FALSE: on SQLite a rename is a
// 12-step table REBUILD, and `render_create_table_sqlite_rebuild` renders the
// new-table CREATE from the TABLE SNAPSHOT — not from the SDK descriptor — for exactly
// the tables that carry a generated column. A stale body therefore reached emitted DDL
// as `GENERATED ALWAYS AS (("qty" * "unit_cents"))` over a table whose column is now
// `quantity`, which SQLite refuses inside the rebuild transaction. See
// `rename_column_generated_expr_snapshot.rs`, which applies it for real, and
// `fold_rename_column_generated_expr_pg.rs`, which shows the server deparsing the NEW
// name. The old assertion was pinning a defect, not a boundary.
//
// The second half is UNCHANGED and still true: the differ does not compare the body.
// That remains correct — live PostgreSQL reports a DEPARSED string and the fold a
// rendered one, so the two spellings would never meet. Following the rename is about
// what the fold EMITS, not about what it can compare.
//
// Each side is asserted SEPARATELY. Asserting only that the differ is quiet would
// pass just as well against a differ that had stopped looking at columns entirely.
#[test]
fn the_snapshot_lane_follows_the_rename_and_the_differ_still_ignores_the_body() {
    let effective = support::confined_charter();

    let folded = fold_ops(
        &[create_line_items(), rename_qty_to_quantity()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("the op stream folds");

    let table = folded
        .tables
        .get("line_items")
        .expect("the folded table is present");
    let total = table
        .columns
        .iter()
        .find(|c| c.name == "total_cents")
        .expect("the generated column survives the rename");

    // Side one: the fold's rendered body names the POST-rename column, which is what
    // PostgreSQL deparses and what the SQLite rebuild has to emit.
    let generated = total
        .generated
        .as_ref()
        .expect("the snapshot carries the generated body");
    assert!(
        generated.expr.contains("quantity") && !generated.expr.contains("qty\""),
        "the rendered body follows the rename to the new column name: {}",
        generated.expr
    );
    // The untouched reference is left alone, so the rewrite is a targeted one rather
    // than a wholesale re-render from something else.
    assert!(
        generated.expr.contains("unit_cents"),
        "an untouched reference survives the rewrite: {}",
        generated.expr
    );
    // And the AST the rendering came from is carried, which is what MAKES the rewrite
    // possible: text substitution is refused everywhere in this crate.
    let source = generated
        .source
        .as_ref()
        .expect("the snapshot keeps the closed Expr its rendering came from");
    let mut names = Vec::new();
    col_refs(
        &serde_json::to_value(source).expect("the closed Expr serializes"),
        &mut names,
    );
    assert!(
        names.iter().any(|n| n == "quantity") && !names.iter().any(|n| n == "qty"),
        "the carried AST names the post-rename column too: {names:?}"
    );

    // Side two: nothing COMPARES it, and that is still deliberate. Two columns
    // differing only in the generated body are equal, because live PostgreSQL reports
    // a deparsed spelling the fold cannot reproduce.
    let mut rewritten = total.clone();
    rewritten.generated = Some(zero_migrate::model::snapshot::GeneratedColumnSnapshot {
        expr: "(\"something\" * \"else\")".to_string(),
        source: None,
        stored: true,
    });
    assert_eq!(
        total, &rewritten,
        "column equality excludes the generated body, so the two renderings never meet"
    );

    // And end to end through the differ, for the same reason.
    let mut other = folded.clone();
    for column in &mut other
        .tables
        .get_mut("line_items")
        .expect("the folded table is present")
        .columns
    {
        if let Some(generated) = column.generated.as_mut() {
            generated.expr = "(\"something\" * \"else\")".to_string();
        }
    }
    let drift = diff_snapshots(&folded, &other);
    assert!(
        drift.is_clean(),
        "a rewritten generated body reports no drift: the comparison is off by design, \
         which is exactly why the fold has to get the EMITTED body right: {drift:?}"
    );
}
