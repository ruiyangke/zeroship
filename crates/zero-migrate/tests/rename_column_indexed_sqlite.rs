//! A fold-seeded SQLite rename of an INDEXED column must apply, and every dependent
//! the rebuild replays must come back over the NEW column name.
//!
//! The SQLite 12-step rebuild captures a table's dependents (its indexes and its
//! triggers) VERBATIM from `sqlite_master` before the `DROP TABLE` and replays them
//! VERBATIM after the swap. The only replay-skip is for columns the rebuild DROPS, and
//! a rename's `from` is deliberately NOT a drop (a rename CARRIES the column). So on the
//! FOLD-seeded leg - where the new table is rendered outright with the POST-rename
//! column name and `SqliteRebuildSpec::column_renames` is empty - a captured
//!
//! ```sql
//! CREATE INDEX "parts_qty_idx" ON "parts" ("qty")
//! ```
//!
//! was replayed against a table whose column is now `amount`, and the rebuild aborted:
//!
//! ```text
//! sqlite rebuild of 'parts' aborted: the dependent index 'parts_qty_idx' could not be
//!   recreated after the rebuild (no such column: "qty" ...); the original table and
//!   its dependents are intact
//! ```
//!
//! Fail-closed and rolled back - so never a lost index, but a rename of ANY indexed
//! column was un-appliable.
//!
//! # Why the repair is a RENAME and not a rewrite
//!
//! The three earlier carriers of this bug shape (generated expressions, inline CHECK
//! bodies, FOREIGN KEY definitions) were all repaired by rewriting rendered SQL the
//! engine itself had produced, with a helper chosen for that text's grammar. A captured
//! `CREATE INDEX` is a THIRD grammar - a bare-or-quoted column list that may hold
//! EXPRESSIONS, `COLLATE`, `ASC`/`DESC`, and a trailing partial `WHERE` predicate - and
//! a captured `CREATE TRIGGER` is a fourth, with a whole statement body. Getting either
//! subtly wrong on a UNIQUE index does not fail: it silently builds the index over the
//! WRONG column and changes what data the table accepts.
//!
//! So no third helper. The executor renames the column on the LIVE table FIRST, with
//! SQLite's own `ALTER TABLE ... RENAME COLUMN`, and captures the dependents AFTER -
//! by which point SQLite's parser has already rewritten every index, trigger and view
//! in the catalog. The replay stays verbatim. This is the same delegation
//! `SqliteRebuildSpec::column_renames` already makes on the CATALOG leg, moved to the
//! other end of the rebuild because on this leg the new table is created with the new
//! name already in place.
//!
//! The tests below therefore assert BEHAVIOUR, not DDL text: `PRAGMA index_info`,
//! a duplicate insert rejected by the rebuilt UNIQUE index, a partial index's predicate
//! actually filtering, and a trigger actually firing.

mod support;

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::ir::IrFlagsOverride;
use zero_migrate::render::fold::single_fold;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, ColType, ExecutorConfig, Migration,
    MigrationEngine, MigrationIr, Op, PlanStep, RenameStep, SqlDialect, SqliteBackend,
    SqliteRebuildSpec, SqliteSequencePolicy,
};

const PROJECT: &str = "prj_indexed_rename";
const APP: &str = "app_indexed_rename";
const TABLE: &str = "parts";
const OLD_COLUMN: &str = "qty";
const NEW_COLUMN: &str = "amount";

const PLAIN_INDEX: &str = "parts_qty_idx";
const UNIQUE_INDEX: &str = "parts_qty_key";
const PARTIAL_INDEX: &str = "parts_label_partial_idx";
const EXPR_INDEX: &str = "parts_qty_expr_idx";

/// The table under rename plus the four index SHAPES a captured `CREATE INDEX` can
/// take: a plain column list, a UNIQUE column list (the correctness trap - a unique
/// index rebuilt over the wrong column changes what rows are accepted), a partial index
/// whose predicate names the column OUTSIDE the key list, and an expression index whose
/// key is not a column reference at all.
fn create_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_parts",
        "owner_app": APP,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    { "name": "id", "type": "text", "nullable": false },
                    { "name": OLD_COLUMN, "type": "int", "nullable": false },
                    { "name": "label", "type": "text", "nullable": false },
                ],
                "primaryKey": ["id"],
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": PLAIN_INDEX,
                "columns": [{ "kind": "column", "name": OLD_COLUMN }],
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": UNIQUE_INDEX,
                "columns": [{ "kind": "column", "name": OLD_COLUMN }],
                "unique": true,
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": PARTIAL_INDEX,
                "columns": [{ "kind": "column", "name": "label" }],
                "where": {
                    "node": "binOp",
                    "op": "gt",
                    "lhs": { "node": "colRef", "name": OLD_COLUMN },
                    "rhs": { "node": "literal", "value": 0 },
                },
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": EXPR_INDEX,
                "columns": [{
                    "kind": "expr",
                    "expr": {
                        "node": "binOp",
                        "op": "add",
                        "lhs": { "node": "colRef", "name": OLD_COLUMN },
                        "rhs": { "node": "literal", "value": 1 },
                    },
                }],
            },
        ],
    }))
    .expect("the create IR deserializes")
}

/// The same table with NO index at all - the fixture the FK/CHECK/generated fixes used,
/// reproduced here so the catalog-leg pin measures the rename and not the index replay.
fn create_ir_without_indexes() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_parts",
        "owner_app": APP,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    { "name": "id", "type": "text", "nullable": false },
                    { "name": OLD_COLUMN, "type": "int", "nullable": false },
                    { "name": "label", "type": "text", "nullable": false },
                ],
                "primaryKey": ["id"],
            },
        ],
    }))
    .expect("the index-free create IR deserializes")
}

/// The table plus ONE plain index, portable across all three dialects.
fn plain_index_create_ir() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_parts",
        "owner_app": APP,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    { "name": "id", "type": "text", "nullable": false },
                    { "name": OLD_COLUMN, "type": "int", "nullable": false },
                    { "name": "label", "type": "text", "nullable": false },
                ],
                "primaryKey": ["id"],
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": PLAIN_INDEX,
                "columns": [{ "kind": "column", "name": OLD_COLUMN }],
            },
        ],
    }))
    .expect("the portable create IR deserializes")
}

fn rename_ir() -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: "rename_qty".to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::RenameColumn {
            table: TABLE.to_string(),
            from: OLD_COLUMN.to_string(),
            to: NEW_COLUMN.to_string(),
            ty: ColType::Int,
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

/// No `[[inject]]`. The three earlier fixes' fixtures all carried a mandatory inject and
/// a column (an enum, a generated expression, an inline CHECK) that routes
/// `render_create_table_sqlite_rebuild` through its SNAPSHOT arm. This one deliberately
/// routes through the OTHER arm - the SDK-value one - which is where an ordinary table
/// with no such column goes, and which is the shape the recorded reproduction used.
/// `an_injected_table_cannot_reach_the_sdk_value_rebuild_arm_at_all` below pins WHY the
/// inject had to come off.
const CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

fn charter() -> zero_migrate::EffectivePolicy {
    zero_migrate::effective_policy_from_charter_toml(CHARTER)
        .expect("the indexed-rename test charter composes")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, charter())
}

/// The SQLite live schema `engine::refresh_historical_live` builds: table snapshots
/// from `fold_ops`, SDK field maps from the `FieldDef` projection, over the same ops. This is
/// the leg with NO `stored_create_sql`, so `preserve_stored_shape` is off and the
/// rebuild renders the new table with the POST-rename column name.
fn folded_live_schema(history: &[Op]) -> LiveSchema {
    let effective = charter();
    let snapshot =
        fold_ops(history, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let sqlite_schemas = single_fold::fold(history, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = sqlite_schemas;
    live
}

fn insert_sql(id: &str, column: &str, qty: i64, label: &str) -> String {
    format!("INSERT INTO main.{TABLE} (id, {column}, label) VALUES ('{id}',{qty},'{label}')")
}

async fn deploy(backend: &SqliteBackend, engine: &MigrationEngine, ir: &MigrationIr) -> Vec<Op> {
    let effective = charter();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create = resolve_create_table_policy(ir, &effective, PROJECT)
        .expect("the create resolves under the charter");
    let steps = author
        .lower_steps(&create, &LiveSchema::default())
        .expect("the create lowers");
    engine
        .apply_plan(
            &steps,
            Approval::None,
            backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .expect("the create applies");
    create.ops
}

/// Lower the rename against the FOLD-seeded live schema and apply it. Returns the
/// backend so the caller can interrogate the real database afterwards.
async fn apply_fold_seeded_rename(
    backend: &SqliteBackend,
    engine: &MigrationEngine,
    create_ops: &[Op],
) -> Result<(), String> {
    let effective = charter();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let live = folded_live_schema(create_ops);
    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the folded live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    // The leg under test: the fold carries no stored CREATE, so the executor gets NO
    // deferred `RENAME COLUMN` and the new table is rendered with the new name already
    // in place. That is precisely why the captured dependents go stale.
    assert!(
        rebuild.spec.column_renames.is_empty(),
        "the FOLD-seeded leg carries no deferred RENAME COLUMN: {:?}",
        rebuild.spec.column_renames
    );
    assert!(
        rebuild.spec.recreate_objects.is_empty(),
        "and no explicit index DDL either - the executor's verbatim capture is the only \
         thing that puts the indexes back: {:?}",
        rebuild.spec.recreate_objects
    );
    assert_eq!(
        rebuild
            .spec
            .copy_columns
            .iter()
            .find(|(dest, _)| dest == NEW_COLUMN)
            .map(|(_, src)| src.as_str()),
        Some(OLD_COLUMN),
        "the copy mapping is the executor's only statement of the rename: {:?}",
        rebuild.spec.copy_columns
    );
    engine
        .apply_plan(
            &steps,
            Approval::Approved,
            backend,
            &exec_cfg(),
            "deploy",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The columns `PRAGMA index_info` reports for `index`, in seqno order. `None` marks a
/// key that is an EXPRESSION rather than a column reference (SQLite reports a NULL name
/// and a -2 rank for those).
async fn index_columns(backend: &SqliteBackend, index: &str) -> Vec<Option<String>> {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .query(&format!("PRAGMA main.index_info({index})"))
        .await
        .expect("read the index columns")
        .into_iter()
        .map(|row| row.get(2).and_then(Clone::clone))
        .collect()
}

async fn index_names(backend: &SqliteBackend) -> Vec<String> {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_schema WHERE type = 'index' AND tbl_name = '{TABLE}' \
             AND sql IS NOT NULL ORDER BY name"
        ))
        .await
        .expect("read the index names")
        .into_iter()
        .filter_map(|row| row.first().and_then(Clone::clone))
        .collect()
}

/// The stored `CREATE INDEX` from the first `ON` onwards - the key list, the
/// attributes and any partial predicate. The index's own NAME is deliberately excluded:
/// it was derived from the pre-rename column (`parts_qty_idx`) and SQLite does not
/// rename an index when a column moves, so the name legitimately keeps saying `qty`.
async fn stored_index_body(backend: &SqliteBackend, index: &str) -> String {
    let sql = stored_index_sql(backend, index).await;
    let on = sql.find(" ON ").unwrap_or_else(|| {
        panic!("a stored CREATE INDEX has an ON clause: {sql}");
    });
    sql[on..].to_string()
}

async fn stored_index_sql(backend: &SqliteBackend, index: &str) -> String {
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let rows = backend
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'index' AND name = '{index}'"
        ))
        .await
        .expect("read the stored index DDL");
    rows.first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .unwrap_or_else(|| panic!("index '{index}' is not in the catalog"))
}

// ---------------------------------------------------------------------------------
// (1) The defect itself: the rename applies, and every index comes back.
// ---------------------------------------------------------------------------------

/// RED before the fix with the DOCUMENTED abort
/// (`the dependent index '...' could not be recreated`), GREEN after.
#[compio::test]
async fn a_fold_seeded_rename_of_an_indexed_column_applies() {
    let p = paths("indexed_rename");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let create_ops = deploy(&backend, &engine, &create_ir()).await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&insert_sql("p1", OLD_COLUMN, 7, "seven"))
        .await
        .expect("seed a row the rebuild has to carry across");

    let before = index_names(&backend).await;
    assert_eq!(
        before,
        vec![
            PARTIAL_INDEX.to_string(),
            EXPR_INDEX.to_string(),
            PLAIN_INDEX.to_string(),
            UNIQUE_INDEX.to_string(),
        ],
        "the four index shapes are live before the rename"
    );

    apply_fold_seeded_rename(&backend, &engine, &create_ops)
        .await
        .expect("a rename of an INDEXED column applies");

    // The data followed the rename.
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let after = backend
        .actor()
        .query(&format!(
            "SELECT {NEW_COLUMN} FROM main.{TABLE} WHERE id = 'p1'"
        ))
        .await
        .expect("read the renamed column");
    assert_eq!(
        after[0][0].as_deref(),
        Some("7"),
        "the seeded value follows the rename"
    );

    // Every dependent came back - none was silently dropped.
    assert_eq!(
        index_names(&backend).await,
        before,
        "every captured index is back on the rebuilt table"
    );

    // And SQLite's OWN structural answer says the plain index is over the NEW column.
    assert_eq!(
        index_columns(&backend, PLAIN_INDEX).await,
        vec![Some(NEW_COLUMN.to_string())],
        "PRAGMA index_info reports the rebuilt index over the POST-rename column"
    );
    let sql = stored_index_body(&backend, PLAIN_INDEX).await;
    assert!(
        sql.contains(NEW_COLUMN) && !sql.contains(OLD_COLUMN),
        "and the stored DDL's key list names only the new column: {sql}"
    );
}

// ---------------------------------------------------------------------------------
// (2) The correctness trap: a UNIQUE index over the WRONG column would be silent.
// ---------------------------------------------------------------------------------

/// A UNIQUE index is the one dependent whose mis-rebuild would not error: an index built
/// over `label` instead of `amount` still exists, still has the right name, and still
/// looks fine in `sqlite_schema` at a glance - it just accepts rows the schema forbids.
/// So this asserts the ENFORCEMENT, both ways.
#[compio::test]
async fn the_rebuilt_unique_index_still_enforces_over_the_renamed_column() {
    let p = paths("indexed_rename_unique");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let create_ops = deploy(&backend, &engine, &create_ir()).await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&insert_sql("p1", OLD_COLUMN, 7, "seven"))
        .await
        .expect("seed a row");
    // Control: the UNIQUE index is enforced on the PRE-rename column.
    backend
        .actor()
        .exec(&insert_sql("dup", OLD_COLUMN, 7, "other"))
        .await
        .expect_err("the control: the unique index rejects a duplicate before the rename");

    apply_fold_seeded_rename(&backend, &engine, &create_ops)
        .await
        .expect("the rename applies");

    assert_eq!(
        index_columns(&backend, UNIQUE_INDEX).await,
        vec![Some(NEW_COLUMN.to_string())],
        "the unique index is over the POST-rename column"
    );

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    // BEHAVIOUR: a duplicate on the RENAMED column is still rejected...
    backend
        .actor()
        .exec(&insert_sql("dup", NEW_COLUMN, 7, "other"))
        .await
        .expect_err(
            "the rebuilt UNIQUE index still rejects a duplicate written to the POST-rename \
             column - an index quietly rebuilt over another column would ACCEPT this",
        );
    // ...and a distinct value is still accepted, so the index is not merely rejecting
    // everything.
    backend
        .actor()
        .exec(&insert_sql("p2", NEW_COLUMN, 8, "eight"))
        .await
        .expect("and a distinct value is accepted");
    // The label column carried a duplicate all along: if the unique index had been
    // rebuilt over `label` instead, the insert above would have been rejected and the
    // one below accepted. This pins WHICH column the enforcement is on.
    backend
        .actor()
        .exec(&insert_sql("p3", NEW_COLUMN, 9, "eight"))
        .await
        .expect("a duplicate LABEL is accepted - the uniqueness did not move to another column");
}

// ---------------------------------------------------------------------------------
// (3) Partial and expression indexes: the column outside the key list.
// ---------------------------------------------------------------------------------

/// `WHERE qty > 0` and `ON parts (qty + 1)` both name the column somewhere a
/// column-list rewrite would never look. A silently un-renamed predicate is a WRONG
/// index, not a failed migration, so these assert the predicate still selects and the
/// expression key still ranks.
#[compio::test]
async fn a_partial_predicate_and_an_expression_key_follow_the_rename() {
    let p = paths("indexed_rename_partial");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let create_ops = deploy(&backend, &engine, &create_ir()).await;
    apply_fold_seeded_rename(&backend, &engine, &create_ops)
        .await
        .expect("the rename applies");

    let partial = stored_index_body(&backend, PARTIAL_INDEX).await;
    assert!(
        partial.contains(&format!("WHERE (\"{NEW_COLUMN}\" > 0)")) && !partial.contains(OLD_COLUMN),
        "the partial predicate names the POST-rename column: {partial}"
    );
    let expr = stored_index_body(&backend, EXPR_INDEX).await;
    assert!(
        expr.contains(NEW_COLUMN) && !expr.contains(OLD_COLUMN),
        "the expression key names the POST-rename column: {expr}"
    );
    assert_eq!(
        index_columns(&backend, EXPR_INDEX).await,
        vec![None],
        "and it is still an EXPRESSION key, not flattened into a column reference"
    );

    // BEHAVIOUR: the partial index's predicate must actually PARTITION the table. Rows
    // on either side of `amount > 0`, then read back through the index.
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&insert_sql("pos", NEW_COLUMN, 5, "in"))
        .await
        .expect("a row inside the predicate");
    backend
        .actor()
        .exec(&insert_sql("neg", NEW_COLUMN, -5, "out"))
        .await
        .expect("a row outside it");
    let planned = backend
        .actor()
        .query(&format!(
            "EXPLAIN QUERY PLAN SELECT label FROM main.{TABLE} \
             WHERE label = 'in' AND {NEW_COLUMN} > 0"
        ))
        .await
        .expect("plan a query the partial index covers");
    let plan = format!("{planned:?}");
    assert!(
        plan.contains(PARTIAL_INDEX),
        "SQLite uses the rebuilt partial index for a query matching its predicate over \
         the POST-rename column: {plan}"
    );
    let rows = backend
        .actor()
        .query(&format!(
            "SELECT label FROM main.{TABLE} WHERE label = 'in' AND {NEW_COLUMN} > 0"
        ))
        .await
        .expect("read through the partial index");
    assert_eq!(
        rows.len(),
        1,
        "and it returns exactly the row inside the predicate: {rows:?}"
    );
}

// ---------------------------------------------------------------------------------
// (4) Triggers. `captured` holds indexes AND triggers, and a trigger body is the
//     harder parse of the two.
// ---------------------------------------------------------------------------------

/// A trigger whose `UPDATE OF` list, `WHEN` clause and body ALL name the renamed column.
/// Nothing in the IR authors a SQLite trigger, so it is created the way a creator's
/// trigger gets there: directly, under `CreatorUp`, before the rename.
#[compio::test]
async fn a_trigger_body_follows_the_rename_too() {
    let p = paths("indexed_rename_trigger");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let create_ops = deploy(&backend, &engine, &create_ir_without_indexes()).await;

    backend
        .actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("mode");
    backend
        .actor()
        .exec("CREATE TABLE main.qty_audit (id TEXT PRIMARY KEY, seen INTEGER NOT NULL)")
        .await
        .expect("an audit table the trigger writes into - not a dependent of the rebuilt table");
    backend
        .actor()
        .exec(&format!(
            "CREATE TRIGGER main.parts_qty_audit AFTER UPDATE OF {OLD_COLUMN} ON {TABLE} \
             WHEN NEW.{OLD_COLUMN} > OLD.{OLD_COLUMN} \
             BEGIN INSERT INTO qty_audit (id, seen) VALUES (NEW.id, NEW.{OLD_COLUMN}); END"
        ))
        .await
        .expect("a creator trigger over the column about to be renamed");

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&insert_sql("p1", OLD_COLUMN, 1, "one"))
        .await
        .expect("seed a row");

    apply_fold_seeded_rename(&backend, &engine, &create_ops)
        .await
        .expect("a rename of a column a TRIGGER names applies");

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let stored = backend
        .actor()
        .query("SELECT sql FROM main.sqlite_schema WHERE type = 'trigger' AND name = 'parts_qty_audit'")
        .await
        .expect("read the rebuilt trigger");
    let stored = stored
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .expect("the trigger survived the rebuild");
    assert!(
        stored.contains(NEW_COLUMN) && !stored.contains(&format!(".{OLD_COLUMN}")),
        "the trigger's UPDATE OF list, WHEN clause and body all name the POST-rename \
         column: {stored}"
    );

    // BEHAVIOUR: the trigger actually fires on an update of the RENAMED column.
    backend
        .actor()
        .exec(&format!(
            "UPDATE main.{TABLE} SET {NEW_COLUMN} = 5 WHERE id = 'p1'"
        ))
        .await
        .expect("update the renamed column");
    let audit = backend
        .actor()
        .query("SELECT seen FROM main.qty_audit WHERE id = 'p1'")
        .await
        .expect("read the audit table");
    assert_eq!(
        audit
            .first()
            .and_then(|row| row.first())
            .and_then(Clone::clone),
        Some("5".to_string()),
        "the rebuilt trigger fired on an UPDATE OF the POST-rename column and recorded \
         its NEW value: {audit:?}"
    );
}

// ---------------------------------------------------------------------------------
// (5) The OTHER leg. A catalog read carries `stored_create_sql`, so the rebuild
//     replays the stored body and defers to SQLite's own RENAME COLUMN afterwards.
//     That leg must not move.
// ---------------------------------------------------------------------------------

/// The catalog leg is where `SqliteRebuildSpec::column_renames` is non-empty, and it is
/// the leg the executor's NEW pre-rename must stay out of: its `copy_columns` are pure
/// identity pairs, so the executor derives NO implied rename from them and the deferred
/// `RENAME COLUMN` stays the only one. Pinned end to end, with an index in place, so a
/// pre-rename leaking into this leg would show up as a duplicate-column or a stale index
/// rather than as an argument.
#[compio::test]
async fn a_catalog_sourced_rename_of_an_indexed_column_still_replays_the_stored_body() {
    let effective = charter();
    let p = paths("indexed_rename_catalog");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let engine = MigrationEngine::new();
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let create_ops = deploy(&backend, &engine, &create_ir()).await;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    backend
        .actor()
        .exec(&insert_sql("p1", OLD_COLUMN, 7, "seven"))
        .await
        .expect("seed a row");

    let snapshot = backend
        .snapshot_schema_sqlite()
        .await
        .expect("the catalog snapshot reads");
    assert!(
        snapshot
            .tables
            .get(TABLE)
            .expect("the deployed table is in the catalog snapshot")
            .stored_create_sql
            .is_some(),
        "the catalog carries the stored CREATE, which is what routes a pure rename \
         through the replay arm"
    );
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = single_fold::fold(&create_ops, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");

    let steps = author
        .lower_steps(&rename_ir(), &live)
        .expect("the rename lowers against the catalog live schema");
    let [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] = steps.as_slice() else {
        panic!("a SQLite renameColumn lowers to exactly one rebuild step: {steps:#?}");
    };
    assert_eq!(
        rebuild.spec.column_renames,
        vec![(OLD_COLUMN.to_string(), NEW_COLUMN.to_string())],
        "the CATALOG leg still defers the rename to SQLite's own RENAME COLUMN: {:?}",
        rebuild.spec
    );
    assert!(
        rebuild
            .spec
            .copy_columns
            .iter()
            .all(|(dest, src)| dest == src),
        "and its copy mapping is pure identity, so the executor derives no implied \
         rename from it: {:?}",
        rebuild.spec.copy_columns
    );
    assert!(
        rebuild.spec.new_table_create.contains(OLD_COLUMN),
        "the emitted CREATE is the stored body, still naming the PRE-rename column: {}",
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
        .expect("the catalog-sourced rebuild applies");

    assert_eq!(
        index_columns(&backend, PLAIN_INDEX).await,
        vec![Some(NEW_COLUMN.to_string())],
        "and the catalog leg's own path still leaves the index over the new column"
    );
    assert_eq!(
        index_columns(&backend, UNIQUE_INDEX).await,
        vec![Some(NEW_COLUMN.to_string())],
        "including the unique one"
    );
    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let after = backend
        .actor()
        .query(&format!(
            "SELECT {NEW_COLUMN} FROM main.{TABLE} WHERE id = 'p1'"
        ))
        .await
        .expect("read the renamed column");
    assert_eq!(
        after[0][0].as_deref(),
        Some("7"),
        "the seeded value followed the catalog-leg rename too"
    );
}

// ---------------------------------------------------------------------------------
// (6) The other dialects. The rebuild is SQLite-only BY ROUTING, not by dialect
//     guards inside it, so the proof is that neither dialect lowers a rename into
//     one.
// ---------------------------------------------------------------------------------

/// PostgreSQL and MySQL rename a column with a native `ALTER TABLE`, so nothing they
/// emit ever reaches the 12-step rebuild this fix changed. Measured by lowering the SAME
/// rename IR under each dialect and asserting the step is NOT a `SqliteRebuild`; the two
/// backends' `rebuild_one` implementations then refuse such a step outright as a routing
/// bug, so the two facts together close the path.
#[test]
fn neither_postgres_nor_mysql_lowers_a_rename_into_a_sqlite_rebuild() {
    let effective = charter();
    // The plain-index fixture, because MySQL refuses a partial predicate outright
    // (`validated createIndex partial predicate on unsupported dialect reached lower`)
    // and the point here is the ROUTING of the rename, not the index surface.
    let create_ir = plain_index_create_ir();
    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
        let author = IrAuthor::new(PROJECT, APP, dialect, &effective);
        let create = resolve_create_table_policy(&create_ir, &effective, PROJECT)
            .expect("the create resolves under the charter");
        let create_steps = author
            .lower_steps(&create, &LiveSchema::default())
            .expect("the create lowers");
        assert!(
            !create_steps
                .iter()
                .any(|s| matches!(s, PlanStep::OnlineRename(RenameStep::SqliteRebuild(_)))),
            "{dialect:?} emits no rebuild for the create"
        );

        let snapshot = fold_ops(&create.ops, dialect, PROJECT, &effective)
            .expect("the history folds under this dialect");
        let live = LiveSchema::from_catalog_snapshot(snapshot, APP);
        // MySQL refuses a LIVE rename lowering outright (`renameColumn is render-only
        // for MySQL, not live-rendered`); PostgreSQL lowers it natively. Neither can
        // produce a rebuild, which is the only thing this test is about.
        match author.lower_steps(&rename_ir(), &live) {
            Ok(steps) => {
                assert!(
                    !steps.is_empty(),
                    "{dialect:?} lowers the rename to something"
                );
                assert!(
                    !steps
                        .iter()
                        .any(|s| matches!(s, PlanStep::OnlineRename(RenameStep::SqliteRebuild(_)))),
                    "{dialect:?} renames a column natively and never routes through the \
                     SQLite 12-step rebuild: {steps:#?}"
                );
            }
            Err(error) => assert_eq!(
                dialect,
                SqlDialect::Mysql,
                "only MySQL declines to lower a live rename at all: {error:?}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------------
// (7) What SQLite ACTUALLY does with each stale dependent, measured on the hardened
//     connection this crate ships rather than reasoned about. This is what decides
//     that the repair had to be a RENAME and not a text rewrite.
// ---------------------------------------------------------------------------------

/// The two halves of the defect are NOT the same failure.
///
/// A stale `CREATE INDEX` is REFUSED: SQLite resolves an index's key list against the
/// table at CREATE time, so the replay fails and the whole rebuild rolls back. Loud.
///
/// A stale `CREATE TRIGGER` is ACCEPTED: SQLite does not resolve a trigger's `UPDATE OF`
/// list, `WHEN` clause or body until the trigger fires, so the replay SUCCEEDS and
/// stores a trigger that can never match again. The update that should have fired it
/// succeeds and NOTHING is written. Silent - which is why the trigger half was the
/// dangerous one, and why no amount of care in a column-list rewrite would have covered
/// it.
///
/// And the stale trigger does not stay local: it poisons `ALTER TABLE ... RENAME COLUMN`
/// for EVERY table in the database, because SQLite reparses the whole schema for that
/// statement. So one silently-staled trigger disables the catalog-leg rename path
/// wholesale.
#[compio::test]
async fn sqlite_refuses_a_stale_index_and_silently_stores_a_stale_trigger() {
    let p = paths("indexed_rename_semantics");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let actor = backend.actor();
    actor.set_mode(Mode::EngineJournal).await.expect("mode");
    actor
        .exec("CREATE TABLE main.t (id TEXT PRIMARY KEY, qty INTEGER)")
        .await
        .expect("the pre-rename table");
    actor
        .exec("CREATE TABLE main.audit (id TEXT, seen INTEGER)")
        .await
        .expect("somewhere for the trigger to write");
    let index_ddl = "CREATE INDEX main.t_qty_idx ON t (qty)";
    let trigger_ddl = "CREATE TRIGGER main.t_qty_audit AFTER UPDATE OF qty ON t \
                       WHEN NEW.qty > OLD.qty \
                       BEGIN INSERT INTO audit (id, seen) VALUES (NEW.id, NEW.qty); END";
    actor.exec(index_ddl).await.expect("an index over qty");
    actor.exec(trigger_ddl).await.expect("a trigger over qty");

    // Stand in for the rebuild: the table comes back with the column renamed, and the
    // captured DDL is replayed verbatim.
    actor.exec("DROP TABLE main.t").await.expect("the swap");
    actor
        .exec("CREATE TABLE main.t (id TEXT PRIMARY KEY, amount INTEGER)")
        .await
        .expect("the post-rename table");

    let index_replay = actor.exec(index_ddl).await.expect_err(
        "a stale index key list is REFUSED - the loud half, and the abort the defect \
         report recorded",
    );
    assert!(
        index_replay.to_string().contains("no such column"),
        "and the refusal names the missing column: {index_replay}"
    );

    actor.exec(trigger_ddl).await.expect(
        "a stale trigger is ACCEPTED - SQLite resolves nothing in a trigger body at \
         CREATE time. This is the half that would have gone unnoticed",
    );
    actor
        .exec("INSERT INTO main.t (id, amount) VALUES ('x', 1)")
        .await
        .expect("a row");
    actor
        .exec("UPDATE main.t SET amount = 5 WHERE id = 'x'")
        .await
        .expect("the update the trigger was written to observe SUCCEEDS");
    let audit = actor
        .query("SELECT id FROM main.audit")
        .await
        .expect("read the audit table");
    assert!(
        audit.is_empty(),
        "and the stale trigger wrote NOTHING - it can never match `UPDATE OF qty` again, \
         and says so nowhere: {audit:?}"
    );

    // The blast radius: a stale trigger anywhere breaks RENAME COLUMN everywhere.
    actor
        .exec("CREATE TABLE main.unrelated (m TEXT)")
        .await
        .expect("a table that has nothing to do with any of this");
    let poisoned = actor
        .exec("ALTER TABLE main.unrelated RENAME COLUMN m TO n")
        .await
        .expect_err(
            "one silently-staled trigger disables RENAME COLUMN for the WHOLE database - \
             SQLite reparses the entire schema for that statement",
        );
    assert!(
        poisoned.to_string().contains("t_qty_audit"),
        "and the error names the stale trigger, not the table being renamed: {poisoned}"
    );
}

/// The refusal the pre-rename has to respect: SQLite will not rename a column onto a
/// name the table still carries. A rebuild is allowed to move a column onto the name of
/// one it is discarding, so the executor must DECLINE such a rename rather than issue it
/// - the rebuild then proceeds exactly as it did before this step existed.
#[compio::test]
async fn a_rename_onto_a_name_the_live_table_still_carries_is_declined_not_forced() {
    let p = paths("indexed_rename_collision");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let actor = backend.actor();

    // First, the raw SQLite refusal, so the executor's guard is measured against the
    // real error rather than an assumption about it.
    actor.set_mode(Mode::EngineJournal).await.expect("mode");
    actor
        .exec("CREATE TABLE main.probe (qty INTEGER, amount INTEGER)")
        .await
        .expect("a table carrying both names");
    let refused = actor
        .exec("ALTER TABLE main.probe RENAME COLUMN qty TO amount")
        .await
        .expect_err("SQLite refuses a rename onto an existing column");
    assert!(
        refused.to_string().contains("duplicate column name"),
        "the refusal is a duplicate-column error: {refused}"
    );

    // Now the rebuild that would have issued it. The new shape keeps ONE `amount`,
    // carrying the OLD `qty` values into it and discarding the old `amount`.
    actor
        .exec("CREATE TABLE main.t (id TEXT PRIMARY KEY, qty INTEGER, amount INTEGER)")
        .await
        .expect("the live shape carries both names");
    actor
        .exec("INSERT INTO main.t (id, qty, amount) VALUES ('x', 7, 99)")
        .await
        .expect("a row that tells the two columns apart");

    let spec = SqliteRebuildSpec {
        table: "t".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("t"),
        new_table_create: "CREATE TABLE \"t__zero_migrate_rebuild\" \
                           (\"id\" TEXT PRIMARY KEY, \"amount\" INTEGER)"
            .into(),
        copy_columns: vec![("id".into(), "id".into()), ("amount".into(), "qty".into())],
        recreate_objects: vec![],
        column_renames: vec![],
        dropped_columns: vec![],
        sequence_policy: SqliteSequencePolicy::Preserve,
        reason: "a rename onto a discarded column's name".into(),
    };
    backend
        .rebuild_one(&spec, &rebuild_migration(&spec), "deployer")
        .await
        .expect(
            "the rebuild still applies: the pre-rename is DECLINED, not forced into a \
             duplicate-column failure",
        );

    actor.set_mode(Mode::EngineJournal).await.expect("mode");
    let rows = actor
        .query("SELECT amount FROM main.t")
        .await
        .expect("read the surviving column");
    assert_eq!(
        rows[0][0].as_deref(),
        Some("7"),
        "and the copy still moved the OLD qty value into the surviving amount column"
    );
}

/// A rebuild that RENAMES one column and DROPS another must keep the dependent over the
/// renamed one and skip the dependent over the dropped one. The pre-rename runs before
/// the capture, so `dropped_columns` is matched against DDL that has already followed
/// the rename - which is sound precisely because a dropped column's name is not one the
/// rename is allowed to move onto.
#[compio::test]
async fn a_rebuild_that_renames_one_column_and_drops_another_keeps_only_the_survivor() {
    let p = paths("indexed_rename_with_drop");
    let backend = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let actor = backend.actor();
    actor.set_mode(Mode::EngineJournal).await.expect("mode");
    actor
        .exec("CREATE TABLE main.t (id TEXT PRIMARY KEY, qty INTEGER, doomed TEXT)")
        .await
        .expect("the live shape");
    actor
        .exec("CREATE INDEX main.t_qty_idx ON t (qty)")
        .await
        .expect("an index over the column that MOVES");
    actor
        .exec("CREATE INDEX main.t_doomed_idx ON t (doomed)")
        .await
        .expect("an index over the column that GOES");
    actor
        .exec("INSERT INTO main.t (id, qty, doomed) VALUES ('x', 7, 'bye')")
        .await
        .expect("a row");

    let spec = SqliteRebuildSpec {
        table: "t".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("t"),
        new_table_create: "CREATE TABLE \"t__zero_migrate_rebuild\" \
                           (\"id\" TEXT PRIMARY KEY, \"amount\" INTEGER)"
            .into(),
        copy_columns: vec![("id".into(), "id".into()), ("amount".into(), "qty".into())],
        recreate_objects: vec![],
        column_renames: vec![],
        dropped_columns: vec!["doomed".into()],
        sequence_policy: SqliteSequencePolicy::Preserve,
        reason: "one rename and one drop".into(),
    };
    backend
        .rebuild_one(&spec, &rebuild_migration(&spec), "deployer")
        .await
        .expect("the mixed rebuild applies");

    actor.set_mode(Mode::EngineJournal).await.expect("mode");
    let names: Vec<String> = actor
        .query(
            "SELECT name FROM main.sqlite_schema WHERE type = 'index' AND tbl_name = 't' \
             AND sql IS NOT NULL ORDER BY name",
        )
        .await
        .expect("read the surviving indexes")
        .into_iter()
        .filter_map(|row| row.first().and_then(Clone::clone))
        .collect();
    assert_eq!(
        names,
        vec!["t_qty_idx".to_string()],
        "the renamed column's index survives; the dropped column's is skipped, not \
         replayed into a failure"
    );
    assert_eq!(
        index_columns(&backend, "t_qty_idx").await,
        vec![Some("amount".to_string())],
        "and the survivor is over the POST-rename column"
    );
    let rows = actor
        .query("SELECT amount FROM main.t")
        .await
        .expect("read the carried value");
    assert_eq!(rows[0][0].as_deref(), Some("7"));
}

// ---------------------------------------------------------------------------------
// (8) NOT this fix's defect, pinned because it is why the fixture above carries no
//     inject - and because it narrows the reported blast radius.
// ---------------------------------------------------------------------------------

/// A SEPARATE defect, in a different layer, found while building the fixture.
///
/// `render_create_table_sqlite_rebuild` has two arms. A table with a generated column,
/// an inline CHECK or a case-insensitive text column goes through the SNAPSHOT renderer;
/// an ordinary table goes through the SDK-VALUE arm, which re-emits from
/// `LiveSchema::sqlite_schemas` - the map the `FieldDef` projection builds, exactly as
/// `engine::refresh_historical_live` does in production. Under a charter with a
/// MANDATORY `[[inject]]`, that map contains the injected columns, and the emitter
/// refuses its own input:
///
/// ```text
/// invalid descriptor: sqlite rebuild emit for 'parts': reserved system field name:
///   Field name 'id' is reserved by the active table-injection policy.
/// ```
///
/// So a fold-seeded rename of an INJECTED, ordinary table never reaches the executor at
/// all - it fails at LOWERING, before any of the index replay this fix is about. The
/// index defect is therefore reachable on exactly two shapes: an injected table with one
/// of the three snapshot-arm facets (which is what the three earlier fixes' fixtures
/// happened to have), and an un-injected table (this file's). This test asserts the
/// CURRENT refusal; whoever fixes the emit should invert it.
#[test]
fn an_injected_table_cannot_reach_the_sdk_value_rebuild_arm_at_all() {
    const INJECTED_CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
]
"#;
    let effective = zero_migrate::effective_policy_from_charter_toml(INJECTED_CHARTER)
        .expect("the injected charter composes");
    // The same table, authored WITHOUT its own `id` (the inject forbids that).
    let create_ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": "create_parts",
        "owner_app": APP,
        "ops": [
            {
                "op": "createTable",
                "name": TABLE,
                "columns": [
                    { "name": OLD_COLUMN, "type": "int", "nullable": false },
                    { "name": "label", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
            {
                "op": "createIndex",
                "table": TABLE,
                "name": PLAIN_INDEX,
                "columns": [{ "kind": "column", "name": OLD_COLUMN }],
            },
        ],
    }))
    .expect("the injected create IR deserializes");

    let create = resolve_create_table_policy(&create_ir, &effective, PROJECT)
        .expect("the create resolves under the injected charter");
    let snapshot =
        fold_ops(&create.ops, SqlDialect::Sqlite, PROJECT, &effective).expect("the history folds");
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, APP);
    live.sqlite_schemas = single_fold::fold(&create.ops, SqlDialect::Sqlite, PROJECT, &effective)
        .map(|folded| folded.project_field_defs())
        .expect("the history folds to field defs");

    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &effective);
    let error = author
        .lower_steps(&rename_ir(), &live)
        .expect_err(
            "PINNED DEFECT, not this fix's: an injected ordinary table cannot lower a \
             fold-seeded SQLite rename at all",
        )
        .to_string();
    assert!(
        error.contains("sqlite rebuild emit") && error.contains("reserved system field name"),
        "and the refusal comes from the SDK-value arm re-emitting the injected columns \
         it was handed: {error}"
    );
}

/// A destructive journal migration for a directly-constructed rebuild spec (the same
/// helper `sqlite_rebuild_apply.rs` uses).
fn rebuild_migration(spec: &SqliteRebuildSpec) -> Migration {
    use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
    let flags = MigrationFlags {
        destructive: true,
        requires_approval: true,
        ..MigrationFlags::default()
    };
    let up = spec.new_table_create.clone();
    let checksum = Checksum::of(&ChecksumInput {
        up: &up,
        down: None,
        flags: &flags,
        owner_app: APP,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: format!("sqlite_rebuild_{}", spec.table),
        up,
        down: None,
        checksum,
        flags,
        owner_app: APP.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}
