//! PR4 code-critic MED-1 regression — `generate_ops` must NOT silently drop a
//! desired snapshot's indexes / constraints (the re-diff-to-zero invariant).
//!
//! Before the fix `synth_delta_ops` only diffed table presence + COLUMNS: a
//! `createTable` was emitted with `indexes: [] , constraints: []` and the
//! existing-table arm added no index/constraint ops — so a schema declaring a
//! plain index, a unique field (→ unique index), an FK, or a CHECK produced a
//! migration that SILENTLY did NOT re-diff to zero, with no error and no TODO.
//!
//! The fix:
//!   - a plain (btree, column-list, non-partial) USER index is SYNTHESIZED as a
//!     standalone `createIndex` op (so re-diff truly reaches zero);
//!   - a non-synthesizable index (non-btree / expression / partial) OR ANY
//!     user-authored constraint (FK / CHECK / unique-constraint) FAILS CLOSED
//!     with `ScaffoldError::UnsupportedIndex` / `UnsupportedConstraint` — loud,
//!     never silently partial;
//!   - platform-managed indexes/constraints (the implicit `<table>_pkey` index +
//!     constraint, the three system-field indexes) are SKIPPED (the CREATE-TABLE
//!     lowering injects them) — they are never re-emitted nor flagged.

use zeroship_migrate::render::declarative::DesiredSchema;
use zeroship_migrate::{
    ColumnSnapshot, ConstraintSnapshot, IndexElement, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use zeroship_migrate::frontend::{generate_ops, ScaffoldError};

const APP: &str = "app_med1";

fn col(name: &str, ty: &str, nullable: bool) -> ColumnSnapshot {
    ColumnSnapshot {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    }
}

fn desired_from(table: &str, t: TableSnapshot) -> DesiredSchema {
    let mut snap = SchemaSnapshot::default();
    snap.tables.insert(table.into(), t);
    DesiredSchema {
        snapshot: snap,
        ownership: [(table.to_string(), APP.to_string())].into_iter().collect(),
        sqlite_schemas: Default::default(),
    }
}

/// A desired table that carries a plain USER btree index over `email` plus the
/// platform-injected `<table>_pkey` index. Generating against an EMPTY live DB must
/// emit the createTable AND a standalone createIndex for the user index — the
/// pkey index is skipped (lowering injects it). PRE-FIX: the user index was
/// silently dropped (no createIndex op at all).
#[test]
fn generate_synthesizes_plain_user_index() {
    let table = "members";
    let t = TableSnapshot {
        columns: vec![
            col("id", "uuid", false),
            col("email", "text", false),
        ],
        indexes: vec![
            // platform-managed — must be skipped, never re-emitted.
            IndexSnapshot::btree("members_pkey", true, vec!["id".into()]),
            // user-authored plain btree index — must be synthesized.
            IndexSnapshot::btree("members_email_idx", false, vec!["email".into()]),
        ],
        constraints: vec![],
        runtime_options: Default::default(),
            comment: None,
        stored_create_sql: None,
    };
    let desired = desired_from(table, t);
    let live = SchemaSnapshot::default();

    let gen = generate_ops("add_members", APP, &desired, &live).expect("generate");

    use zeroship_migrate::model::ir::Op;
    let create_indexes: Vec<_> = gen
        .ir
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::CreateIndex { table, columns, name, .. } => {
                Some((table.clone(), columns.clone(), name.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        create_indexes.len(),
        1,
        "exactly ONE user index must be synthesized (pkey skipped); got {:?}",
        gen.ir.ops
    );
    let (idx_table, idx_cols, idx_name) = &create_indexes[0];
    assert_eq!(idx_table, "members");
    assert_eq!(
        idx_cols,
        &vec![IndexElement::Column {
            name: "email".to_string(),
            order: None,
        }]
    );
    assert_eq!(idx_name.as_deref(), Some("members_email_idx"));
    // The emitted .ts mirrors the synthesized index via the fluent surface:
    // `table("members").index("members_email_idx").add({ columns: ["email"] })`.
    assert!(
        gen.ts_body.contains(".index(") && gen.ts_body.contains(".add({"),
        "the emitted .ts must mirror the synthesized index via .index().add(); got:\n{}",
        gen.ts_body
    );
}

/// A desired table carrying a USER FOREIGN KEY constraint (only `definition` text,
/// not structured operands) must FAIL CLOSED — never silently dropped. PRE-FIX the
/// constraint was ignored and generate returned Ok with a partial migration.
#[test]
fn generate_fails_closed_on_user_constraint() {
    let table = "orders";
    let t = TableSnapshot {
        columns: vec![col("id", "uuid", false), col("user_id", "uuid", false)],
        indexes: vec![IndexSnapshot::btree("orders_pkey", true, vec!["id".into()])],
        constraints: vec![
            // platform-managed — skipped.
            ConstraintSnapshot {
                name: "orders_pkey".into(),
                kind: "PRIMARY KEY".into(),
                definition: "PRIMARY KEY (id)".into(),
                comment: None,
            },
            // user FK — definition text only; must fail closed.
            ConstraintSnapshot {
                name: "user_id_fkey".into(),
                kind: "FOREIGN KEY".into(),
                definition: "FOREIGN KEY (user_id) REFERENCES \"app\".users(id)".into(),
                comment: None,
            },
        ],
        runtime_options: Default::default(),
        comment: None,
        stored_create_sql: None,
    };
    let desired = desired_from(table, t);
    let live = SchemaSnapshot::default();

    let err = generate_ops("add_orders", APP, &desired, &live)
        .expect_err("a user constraint generate cannot synthesize must fail closed");
    match err {
        ScaffoldError::UnsupportedConstraint { table, name, .. } => {
            assert_eq!(table, "orders");
            assert_eq!(name, "user_id_fkey");
        }
        other => panic!("expected UnsupportedConstraint, got {other:?}"),
    }
}

/// A desired table carrying a non-btree (vector ANN) index must FAIL CLOSED — its
/// expression/opclass shape is not synthesizable as a portable `createIndex`.
#[test]
fn generate_fails_closed_on_non_btree_index() {
    let table = "docs";
    let mut ann = IndexSnapshot::btree("docs_embedding_idx", false, vec!["embedding".into()]);
    ann.access_method = "ivfflat".into();
    let t = TableSnapshot {
        columns: vec![col("id", "uuid", false), col("embedding", "text", false)],
        indexes: vec![
            IndexSnapshot::btree("docs_pkey", true, vec!["id".into()]),
            ann,
        ],
        constraints: vec![],
        runtime_options: Default::default(),
            comment: None,
        stored_create_sql: None,
    };
    let desired = desired_from(table, t);
    let live = SchemaSnapshot::default();

    let err = generate_ops("add_docs", APP, &desired, &live)
        .expect_err("a non-btree index must fail closed");
    match err {
        ScaffoldError::UnsupportedIndex { table, name, .. } => {
            assert_eq!(table, "docs");
            assert_eq!(name, "docs_embedding_idx");
        }
        other => panic!("expected UnsupportedIndex, got {other:?}"),
    }
}

/// The constraint-free / index-free baseline still works (no regression to the
/// existing column-only generate path).
#[test]
fn generate_plain_table_still_works() {
    let table = "widgets";
    let t = TableSnapshot {
        columns: vec![col("id", "uuid", false), col("label", "text", false)],
        indexes: vec![IndexSnapshot::btree("widgets_pkey", true, vec!["id".into()])],
        constraints: vec![ConstraintSnapshot {
            name: "widgets_pkey".into(),
            kind: "PRIMARY KEY".into(),
            definition: "PRIMARY KEY (id)".into(),
            comment: None,
        }],
        runtime_options: Default::default(),
        comment: None,
        stored_create_sql: None,
    };
    let desired = desired_from(table, t);
    let live = SchemaSnapshot::default();
    let gen = generate_ops("add_widgets", APP, &desired, &live).expect("generate");
    assert!(!gen.is_empty);
    use zeroship_migrate::model::ir::Op;
    // Exactly ONE createTable, NO standalone createIndex (pkey skipped).
    assert_eq!(
        gen.ir.ops.iter().filter(|o| matches!(o, Op::CreateIndex { .. })).count(),
        0
    );
    assert_eq!(
        gen.ir.ops.iter().filter(|o| matches!(o, Op::CreateTable { .. })).count(),
        1
    );
}
