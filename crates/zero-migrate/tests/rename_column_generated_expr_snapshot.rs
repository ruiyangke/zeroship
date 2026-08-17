//! A column rename must follow the generated expressions that READ that column,
//! in the SNAPSHOT lane — the one `fold_ops` builds and the one the SQLite rebuild
//! emits DDL from.
//!
//! The descriptor lane already does this (`fold_rename_column_generated_expr_runtime`).
//! The snapshot lane did not, and the reason given was that nothing reads the folded
//! body. That exclusion is a testable claim, and it is FALSE on SQLite: a rename is a
//! 12-step table REBUILD, and `render_create_table_sqlite_rebuild` renders the
//! new-table `CREATE` from the TABLE SNAPSHOT — not from the SDK descriptor — for
//! exactly the tables that have a generated column (`has_generated_or_identity`). A
//! snapshot whose generated body still names the pre-rename column therefore emits
//!
//! ```sql
//! CREATE TABLE "line_items__zero_migrate_rebuild" (
//!   "quantity" INTEGER,
//!   "total_cents" INTEGER GENERATED ALWAYS AS (("qty_on_hand" + 1)) STORED)
//! ```
//!
//! naming a column the new table does not have. SQLite refuses it, inside the
//! rebuild transaction, so the migration cannot apply at all.
//!
//! The live schema used here is the one `engine::refresh_historical_live` builds for
//! SQLite: table snapshots from `fold_ops`, SDK schemas from `fold_to_field_defs`.
//! That is a production shape, not a fixture convenience — it is what the SQLite
//! rerun path (`lower_completed_historical`) lowers against.
//!
//! PostgreSQL is the oracle for WHICH side is right. It holds a generated expression
//! as a parse tree over attribute NUMBERS, so `pg_get_expr` deparses the NEW name the
//! instant the rename commits. The second test measures that against the fold rather
//! than asserting it from the docs.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::IrFlagsOverride;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{
    fold_ops, fold_to_field_defs, resolve_create_table_policy, Approval, BinaryOp, ColType,
    ExecutorConfig, Expr, GeneratedCol, IrColumn, IrScalar, MigrationEngine, MigrationIr, Op,
    SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_gen_rename";
const APP: &str = "app_gen_rename";
const TABLE: &str = "line_items";
const OLD_COLUMN: &str = "qty_on_hand";
const NEW_COLUMN: &str = "quantity";
const GENERATED_COLUMN: &str = "total_cents";

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

/// `qty_on_hand + 1`, the expression whose reference the rename has to follow.
fn generated_from_old_column() -> GeneratedCol {
    GeneratedCol {
        expr: Expr::BinOp {
            op: BinaryOp::Add,
            lhs: Box::new(Expr::col(OLD_COLUMN)),
            rhs: Box::new(Expr::Literal {
                value: IrScalar::Int(1),
            }),
        },
        stored: true,
    }
}

fn create_ir() -> MigrationIr {
    let mut generated = col(GENERATED_COLUMN, ColType::Int);
    generated.generated = Some(generated_from_old_column());
    ir(
        "create_line_items",
        vec![Op::CreateTable {
            name: TABLE.to_string(),
            columns: vec![col(OLD_COLUMN, ColType::Int), generated],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }],
    )
}

fn rename_ir() -> MigrationIr {
    ir(
        "rename_qty_on_hand",
        vec![Op::RenameColumn {
            table: TABLE.to_string(),
            from: OLD_COLUMN.to_string(),
            to: NEW_COLUMN.to_string(),
            ty: ColType::Int,
            schema: None,
            existence_guard: None,
        }],
    )
}

fn ir(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: name.to_string(),
        owner_app: APP.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::confined_charter())
}

/// The SQLite live schema `engine::refresh_historical_live` builds: table snapshots
/// from `fold_ops`, SDK field maps from `fold_to_field_defs`, over the same ops.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let effective = support::confined_charter();
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let sqlite_schemas = fold_to_field_defs(history, SqlDialect::Sqlite, PROJECT, &effective)
        .expect("the history folds to field defs");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = sqlite_schemas;
    live.unique_indexes = BTreeSet::new();
    live
}

// The SQLite rebuild renders its new-table CREATE from the folded TABLE SNAPSHOT
// whenever the table carries a generated column, so a snapshot whose generated body
// still names the pre-rename column emits a CREATE the engine cannot execute. Applied
// for real: the rebuild has to run, the row has to survive, and the recomputed
// generated value has to follow the renamed column.
#[compio::test]
async fn a_sqlite_rename_rebuild_emits_a_generated_body_over_the_new_column_name() {
    let effective = support::confined_charter();
    let p = paths("gen_rename");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);

    // Deploy the table for real, and keep the RESOLVED ops as the fold's history —
    // the same cumulative-op list the engine folds.
    let create = resolve_create_table_policy(&create_ir(), &effective, PROJECT)
        .expect("the create resolves under the charter");
    let steps = author
        .lower_steps(&create, &LiveSchema::default())
        .expect("the create lowers");
    engine
        .apply_plan(
            &steps,
            Approval::None,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the create applies");

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&format!(
            "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, {OLD_COLUMN}) \
             VALUES ('l1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,41)"
        ))
        .await
        .expect("seed a row the rebuild has to carry across");

    // Control: the generated column computes from the PRE-rename column.
    let before = backend
        .actor()
        .query(&format!(
            "SELECT {GENERATED_COLUMN} FROM main.{TABLE} WHERE id = 'l1'"
        ))
        .await
        .expect("read the generated value before the rename");
    assert_eq!(
        before[0][0].as_deref(),
        Some("42"),
        "the control: the generated column computes qty_on_hand + 1 before the rename"
    );

    let live = folded_live_schema(&create.ops);
    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the folded live schema");

    // The emitted CREATE must not name the renamed-away column. Asserted on the SQL
    // before the apply, so a failure names the defect rather than reporting an opaque
    // SQLite error. Only the CREATE is inspected: the rebuild's copy mapping names the
    // old column CORRECTLY (`("quantity", "qty_on_hand")` — the SELECT reads the live
    // table, which has not been renamed yet), so a whole-step text search would pass
    // for the wrong reason and fail for a right one.
    let spec = match &steps[..] {
        [zero_migrate::PlanStep::OnlineRename(zero_migrate::RenameStep::SqliteRebuild(rebuild))] => {
            &rebuild.spec
        }
        other => panic!("a SQLite rename lowers to exactly one rebuild step, got {other:?}"),
    };
    assert!(
        !spec.new_table_create.contains(OLD_COLUMN),
        "the rebuild's CREATE still names the renamed-away column, so the new table \
         declares a generated expression over a column it does not have: {}",
        spec.new_table_create
    );
    assert!(
        spec.new_table_create
            .contains(&format!("GENERATED ALWAYS AS ((\"{NEW_COLUMN}\" + 1))")),
        "the generated body follows the rename to the new column name: {}",
        spec.new_table_create
    );
    // SQLite refuses a write to a generated column, so the copy phase must recompute
    // it rather than carry it across.
    assert!(
        !spec
            .copy_columns
            .iter()
            .any(|(dest, _)| dest == GENERATED_COLUMN),
        "a generated column must not appear in the rebuild's copy list: {:?}",
        spec.copy_columns
    );

    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the rebuild applies");

    // The row survived, and the generated column recomputes from the NEW name.
    let after = backend
        .actor()
        .query(&format!(
            "SELECT {NEW_COLUMN}, {GENERATED_COLUMN} FROM main.{TABLE} WHERE id = 'l1'"
        ))
        .await
        .expect("read the renamed column and its generated peer");
    assert_eq!(
        after[0][0].as_deref(),
        Some("41"),
        "the seeded value follows the rename"
    );
    assert_eq!(
        after[0][1].as_deref(),
        Some("42"),
        "the generated column still computes from the column it was authored over"
    );

    // And the stored schema names the new column inside the generated clause.
    let stored = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = '{TABLE}'"
        ))
        .await
        .expect("read the stored CREATE");
    let stored = stored[0][0].as_deref().expect("the stored CREATE text");
    assert!(
        stored.contains(NEW_COLUMN) && !stored.contains(OLD_COLUMN),
        "the rebuilt table's stored CREATE names only the post-rename column: {stored}"
    );
}
