//! A column rename must follow the INLINE CHECK bodies that name that column.
//!
//! `ColumnSnapshot::inline_checks` is the enum / domain / UUID / TypeID-ULID
//! membership and format predicates, rendered to SQL text at fold time. Every one of
//! them names ITS OWN column. `sqlite_rename_rebuild` derives the post-rename table
//! from the LIVE one by rewriting `ColumnSnapshot::name`, so before this fix the
//! rebuilt `CREATE TABLE` said
//!
//! ```sql
//! "state" TEXT NOT NULL CHECK ("status" IN ('UNCONFIRMED','CONFIRMED','status'))
//! ```
//!
//! naming a column the new table does not have. The `SqliteBackend` opens with
//! double-quoted string literals OFF for DDL and DML (`SQLITE_DBCONFIG_DQS_DDL` /
//! `_DQS_DML`), so SQLite REFUSES that CREATE rather than degrading the unknown
//! identifier to a constant string - the rebuild's leading statement, so the whole
//! migration fails inside its transaction before any value is copied. That is the
//! failure this measures, and it is a failed migration rather than a corrupted
//! schema only because of that hardening.
//!
//! The live schema used here is the one `engine::refresh_historical_live` builds for
//! SQLite - table snapshots from `fold_ops`, SDK schemas from the `FieldDef` projection -
//! which is the shape whose `inline_checks` are populated at all. A SQLite CATALOG
//! read leaves the field EMPTY (`apply::backend::sqlite::drift_sql`), so the
//! catalog-sourced rebuild renders from the SDK descriptor or replays the stored
//! body; the last test here pins that leg so the fix cannot start rewriting a body
//! it is meant to leave alone.
//!
//! The fixture is built to catch the two ways naive text substitution gets this
//! wrong: an enum MEMBER LITERAL spelling the pre-rename column name (`'status'`),
//! and a SIBLING column whose name has the renamed one as a PREFIX (`status_id`).

use crate::support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::IrFlagsOverride;
use zero_migrate::render::fold::single_fold;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, ColType, ExecutorConfig, MigrationEngine,
    MigrationIr, Op, PlanStep, RenameStep, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_inline_check";
const APP: &str = "app_inline_check";
const TABLE: &str = "issues";
const OLD_COLUMN: &str = "status";
const NEW_COLUMN: &str = "state";
/// A sibling whose name HAS the renamed column's name as a prefix. A substring
/// rewrite would turn its own predicate into one over `state_id`.
const SIBLING_COLUMN: &str = "status_id";

/// `createEnum` + a table typed by it. One enum MEMBER deliberately spells the
/// pre-rename COLUMN name, so a rewrite that cannot tell an identifier from a string
/// literal corrupts the membership set instead of the column reference.
fn create_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_issues",
        "owner_app": APP,
        "ops": [
            {
                "op": "createEnum",
                "name": "issue_status",
                "values": ["UNCONFIRMED", "CONFIRMED", "status"],
            },
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    {
                        "name": OLD_COLUMN,
                        "type": { "enum": { "name": "issue_status" } },
                        "nullable": false,
                    },
                    { "name": SIBLING_COLUMN, "type": "text", "nullable": true },
                    { "name": "summary", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ],
    }))
    .expect("the create IR deserializes")
}

fn rename_ir() -> MigrationIr {
    rename_ir_named("rename_status", OLD_COLUMN, NEW_COLUMN)
}

fn rename_ir_named(name: &str, from: &str, to: &str) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: name.to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::RenameColumn {
            table: TABLE.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            ty: ColType::Text,
            schema: None,
            existence_guard: None,
        }],
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
/// from `fold_ops`, SDK field maps from the `FieldDef` projection, over the same ops.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let effective = support::confined_charter();
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let sqlite_schemas = single_fold::fold(history, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = sqlite_schemas;
    live.unique_indexes = BTreeSet::new();
    live
}

fn insert_sql(state: &str) -> String {
    format!(
        "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, {NEW_COLUMN}, \
         {SIBLING_COLUMN}, summary) VALUES ('i2','2026-01-01T00:00:00Z',\
         '2026-01-01T00:00:00Z',1,'{state}','s-2','second')"
    )
}

#[compio::test]
async fn a_sqlite_rename_rebuild_emits_a_check_body_over_the_new_column_name() {
    let effective = support::confined_charter();
    let p = paths("inline_check");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);

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
            "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, {OLD_COLUMN}, \
             {SIBLING_COLUMN}, summary) VALUES ('i1','2026-01-01T00:00:00Z',\
             '2026-01-01T00:00:00Z',1,'CONFIRMED','s-1','first')"
        ))
        .await
        .expect("seed a row the rebuild has to carry across");

    // Control: the membership CHECK is live and enforced on the PRE-rename column.
    let rejected = backend
        .actor()
        .exec(&format!(
            "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, {OLD_COLUMN}, \
             summary) VALUES ('bad','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,\
             'NOT_A_MEMBER','x')"
        ))
        .await;
    assert!(
        rejected.is_err(),
        "the control: the membership CHECK rejects a non-member before the rename"
    );

    let live = folded_live_schema(&create.ops);
    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the folded live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    let create_sql = rebuild.spec.new_table_create.clone();

    // Asserted on the SQL before the apply, so a failure NAMES the defect instead of
    // reporting an opaque SQLite error. Only the CREATE is inspected: the rebuild's
    // copy mapping names the old column correctly (the SELECT reads the not-yet-
    // renamed live table), so a whole-step search would pass for the wrong reason.
    assert!(
        create_sql.contains(&format!(
            r#""{NEW_COLUMN}" TEXT NOT NULL CHECK ("{NEW_COLUMN}" IN ("#
        )),
        "the inline membership CHECK names the POST-rename column:\n{create_sql}"
    );
    assert!(
        !create_sql.contains(&format!(r#"CHECK ("{OLD_COLUMN}" IN ("#)),
        "no CHECK body may still name the renamed-away column - the new table does \
         not have it:\n{create_sql}"
    );
    // The enum member that SPELLS the old column name is a string literal, not a
    // column reference, and must survive untouched.
    assert!(
        create_sql.contains("'UNCONFIRMED', 'CONFIRMED', 'status'"),
        "the membership set is preserved verbatim, including the member that spells \
         the pre-rename column name:\n{create_sql}"
    );
    // The sibling whose name has the renamed one as a PREFIX is untouched.
    assert!(
        create_sql.contains(&format!(r#""{SIBLING_COLUMN}""#)),
        "the prefix-sharing sibling column keeps its own name:\n{create_sql}"
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

    // The row survived under the new column name.
    let after = backend
        .actor()
        .query(&format!(
            "SELECT {NEW_COLUMN}, {SIBLING_COLUMN} FROM main.{TABLE} WHERE id = 'i1'"
        ))
        .await
        .expect("read the renamed column");
    assert_eq!(
        after[0][0].as_deref(),
        Some("CONFIRMED"),
        "the seeded value follows the rename"
    );
    assert_eq!(
        after[0][1].as_deref(),
        Some("s-1"),
        "the prefix-sharing sibling's value is untouched"
    );

    // BEHAVIOUR, not spelling: the rebuilt CHECK ENFORCES membership on the new
    // column. A body naming a column the table does not have could not do this.
    let rejected = backend.actor().exec(&insert_sql("NOT_A_MEMBER")).await;
    assert!(
        rejected.is_err(),
        "the rebuilt CHECK rejects a non-member written to the POST-rename column"
    );
    backend
        .actor()
        .exec(&insert_sql("status"))
        .await
        .expect("the member that spells the pre-rename column name is still a member");

    // And the stored CREATE names only the post-rename column inside the CHECK.
    let stored = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = '{TABLE}'"
        ))
        .await
        .expect("read the stored CREATE");
    let stored = stored[0][0].as_deref().expect("the stored CREATE text");
    assert!(
        stored.contains(&format!(r#"CHECK ("{NEW_COLUMN}" IN ("#)),
        "the rebuilt table's stored CREATE carries the membership over the new \
         column: {stored}"
    );
}

/// The OTHER leg, which must not start being rewritten. A SQLite CATALOG read leaves
/// `inline_checks` EMPTY and `stored_create_sql` SET, so a pure rename replays the
/// stored body byte-for-byte and lets SQLite's own `RENAME COLUMN` rewrite the
/// predicate. Pinned so the fix above cannot leak into it.
#[compio::test]
async fn a_catalog_sourced_rename_still_replays_the_stored_body() {
    let effective = support::confined_charter();
    let p = paths("inline_check_catalog");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);

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

    // The production `live` for a SQLite deploy: a CATALOG snapshot, supplemented
    // with the SDK field map (`MigrationEngine::deploy`).
    let snapshot = backend
        .snapshot_schema_sqlite()
        .await
        .expect("the catalog snapshot reads");
    let table = snapshot
        .tables
        .get(TABLE)
        .expect("the deployed table is in the catalog snapshot");
    assert!(
        table
            .columns
            .iter()
            .all(|column| column.inline_checks.is_empty()),
        "a catalog read carries no inline_checks at all - the field the rename \
         rewrite acts on is EMPTY on this leg: {:?}",
        table.columns
    );
    assert!(
        table.stored_create_sql.is_some(),
        "and it carries the stored CREATE, which is what routes a pure rename \
         through the replay arm"
    );

    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = single_fold::fold(&create.ops, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");

    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the catalog live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    let create_sql = &rebuild.spec.new_table_create;
    assert!(
        create_sql.contains(&format!(
            r#""{OLD_COLUMN}" TEXT NOT NULL CHECK ("{OLD_COLUMN}" IN ("#
        )),
        "the replay arm reproduces the STORED body verbatim, still naming the \
         pre-rename column - SQLite's own RENAME COLUMN rewrites it after the swap, \
         and the rename rewrite must not touch this:\n{create_sql}"
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
        .expect("the catalog-sourced rebuild applies");

    let stored = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = '{TABLE}'"
        ))
        .await
        .expect("read the stored CREATE");
    let stored = stored[0][0].as_deref().expect("the stored CREATE text");
    assert!(
        stored.contains(&format!(r#"CHECK ("{NEW_COLUMN}" IN ("#)),
        "SQLite's RENAME COLUMN rewrote the predicate on this leg: {stored}"
    );
    let rejected = backend.actor().exec(&insert_sql("NOT_A_MEMBER")).await;
    assert!(
        rejected.is_err(),
        "and the replayed CHECK enforces membership on the post-rename column"
    );
}

/// TWO renames in one history, which is why the FOLD's `Op::RenameColumn` arm has to
/// follow the CHECK body too and not only the rebuild.
///
/// The rebuild rewrites the rename IT IS LOWERING. The live snapshot it starts from is
/// `fold_ops` over the cumulative history, so once that history contains an earlier
/// rename the body handed to the second rebuild is already stale in a way the second
/// rebuild cannot see: it is looking for `state`, and the body says `status`. Fixing
/// only the rebuild leaves this broken, so it is measured end to end rather than
/// reasoned about.
#[compio::test]
async fn a_second_rename_starts_from_a_folded_body_the_first_rename_already_moved() {
    let effective = support::confined_charter();
    let p = paths("inline_check_twice");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);

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

    let first = rename_ir();
    let steps = author
        .lower_steps(&first, &folded_live_schema(&create.ops))
        .expect("the first rename lowers");
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
        .expect("the first rebuild applies");

    // The cumulative history the engine folds after the first envelope.
    let mut history = create.ops.clone();
    history.extend(first.ops.iter().cloned());

    // The FOLD's own output, before any rebuild touches it.
    let folded =
        fold_ops(&history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let column = folded.tables[TABLE]
        .columns
        .iter()
        .find(|column| column.name == NEW_COLUMN)
        .expect("the renamed column is in the folded snapshot");
    assert_eq!(
        column.inline_checks.len(),
        1,
        "the folded column carries its membership CHECK: {:?}",
        column.inline_checks
    );
    assert!(
        column.inline_checks[0].contains(&format!(r#"CHECK ("{NEW_COLUMN}" IN ("#))
            && !column.inline_checks[0].contains(&format!(r#""{OLD_COLUMN}" IN ("#)),
        "the FOLD followed the rename into the CHECK body: {:?}",
        column.inline_checks
    );
    assert!(
        column.inline_checks[0].contains("'status'"),
        "and left the member literal that spells the pre-rename column alone: {:?}",
        column.inline_checks
    );

    // And the second rename, lowered against that folded live schema and applied.
    let second = rename_ir_named("rename_state", NEW_COLUMN, "condition");
    let steps = author
        .lower_steps(&second, &folded_live_schema(&history))
        .expect("the second rename lowers");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    assert!(
        rebuild
            .spec
            .new_table_create
            .contains(r#""condition" TEXT NOT NULL CHECK ("condition" IN ("#),
        "the second rebuild's CHECK names the twice-renamed column:\n{}",
        rebuild.spec.new_table_create
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
        .expect("the second rebuild applies");

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let rejected = backend
        .actor()
        .exec(&format!(
            "INSERT INTO main.{TABLE} (id, created_at, updated_at, version, condition, summary) \
             VALUES ('i3','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'NOT_A_MEMBER','third')"
        ))
        .await;
    assert!(
        rejected.is_err(),
        "and the twice-moved CHECK still enforces membership"
    );
}
